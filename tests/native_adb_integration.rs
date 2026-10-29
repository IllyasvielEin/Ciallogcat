//! Actual native fixture executable, shared by Linux and Windows CI.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

const WAIT: Duration = Duration::from_secs(20);
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/native-adb-integration")
            .join(format!("fixture-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let executable = directory.join(format!("native fake adb{}", std::env::consts::EXE_SUFFIX));
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let mut child = OwnedChild(
            Command::new(compiler)
                .args(["--edition=2024", "--crate-name", "native_fake_adb"])
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_fake_adb.rs"),
                )
                .arg("-o")
                .arg(&executable)
                .stdout(File::create(directory.join("rustc.out")).unwrap())
                .stderr(File::create(directory.join("rustc.err")).unwrap())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fixture compilation: {}",
                    fs::read_to_string(directory.join("rustc.err")).unwrap()
                );
                break;
            }
            assert!(Instant::now() < until, "native fixture rustc timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
        executable
    })
}
struct Sandbox {
    root: PathBuf,
    adb: PathBuf,
    scenario: &'static str,
}
impl Sandbox {
    fn new(scenario: &'static str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/native-adb-integration")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(root.join("trace")).unwrap();
        let adb = root.join(format!("fake adb{}", std::env::consts::EXE_SUFFIX));
        fs::copy(fixture(), &adb).unwrap();
        Self {
            root,
            adb,
            scenario,
        }
    }
    fn path(&self, name: &str) -> String {
        self.root.join(name).to_str().unwrap().into()
    }
    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let id = uuid::Uuid::new_v4();
        let out = self.root.join(format!("{id}.out"));
        let err = self.root.join(format!("{id}.err"));
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args(args)
                .env("CIALLOGCAT_ADB", &self.adb)
                .env("CIALLOGCAT_CONFIG_DIR", self.root.join("config"))
                .env("CIALLOGCAT_NATIVE_SCENARIO", self.scenario)
                .env("CIALLOGCAT_NATIVE_TRACE", self.root.join("trace"))
                .stdin(Stdio::null())
                .stdout(File::create(&out).unwrap())
                .stderr(File::create(&err).unwrap())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until, "CLI timed out: {args:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        (
            status.success(),
            fs::read_to_string(out).unwrap(),
            fs::read_to_string(err).unwrap(),
        )
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (ok, out, err) = self.run(args);
        assert!(ok, "{args:?}: {err}");
        serde_json::from_str(&out).unwrap()
    }
    fn ready_pids(&self) -> Vec<u32> {
        fs::read_dir(self.root.join("trace"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let text = fs::read_to_string(&path).unwrap_or_default();
                text.lines()
                    .any(|s| s == "stream_ready")
                    .then(|| path.file_stem().unwrap().to_str().unwrap().parse().unwrap())
            })
            .collect()
    }
    fn wait_ready(&self) -> Vec<u32> {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            let pids = self.ready_pids();
            if !pids.is_empty() {
                return pids;
            }
            assert!(Instant::now() < until, "fixture never ready");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.root.join("session/status.json").is_file() {
            let _ = self.run(&[
                "stop",
                "--session-dir",
                &self.path("session"),
                "--wait-secs",
                "2",
            ]);
        }
        // Watchdog is the final fallback; never terminate persisted trace PIDs.
    }
}

#[cfg(windows)]
struct ProcessObserver(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl ProcessObserver {
    fn open(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        // SAFETY: Request only wait access to the freshly observed fixture PID.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(
            !handle.is_null(),
            "OpenProcess failed: {}",
            std::io::Error::last_os_error()
        );
        Self(handle)
    }
    fn assert_exited(&self) {
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: Observer owns a valid process handle until Drop.
        assert_eq!(
            unsafe { WaitForSingleObject(self.0, 3000) },
            0,
            "owned fixture still alive"
        );
    }
}
#[cfg(windows)]
impl Drop for ProcessObserver {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
#[cfg(not(windows))]
struct ProcessObserver(u32);
#[cfg(not(windows))]
impl ProcessObserver {
    fn open(pid: u32) -> Self {
        Self(pid)
    }
    fn assert_exited(&self) {
        #[cfg(target_os = "linux")]
        {
            let until = Instant::now() + Duration::from_secs(3);
            while Path::new(&format!("/proc/{}", self.0)).exists() {
                assert!(Instant::now() < until, "fixture was not reaped");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self.0;
        }
    }
}

#[test]
fn native_snapshot_scope_completes_text_and_binary_streams_without_reconnect() {
    for (scenario, transport) in [("snapshot", "text"), ("proto_normal", "proto")] {
        for scope in [
            vec![],
            vec!["--tail", "3"],
            vec!["--since", "2024-02-29 12:00:00.123"],
        ] {
            let sandbox = Sandbox::new(scenario);
            let mut args = vec![
                "capture",
                "--serial",
                "TEST-USB",
                "--transport",
                transport,
                "--dump",
                "--buffers",
                "events,radio",
                "--max-bytes",
                "8192",
                "--duration-secs",
                "10",
            ];
            args.extend(scope);
            let start = Instant::now();
            let (ok, out, err) = sandbox.run(&args);
            assert!(ok, "{err}");
            assert!(start.elapsed() < Duration::from_secs(3));
            let status: Value = serde_json::from_str(err.trim()).unwrap();
            assert_eq!(status["reason"], "snapshot_complete");
            assert_eq!(status["stream_interruptions"], 0);
            assert_eq!(status["entries"], 1);
            assert_eq!(status["record_schema_version"], 3);
            assert_eq!(status["visible_end"]["offset"], out.len());
            assert_eq!(status["visible_end"]["seq"], 1);
            assert!(out.len() <= 8192);
            let doc = ciallogcat::document::load(
                out.as_bytes(),
                4 * 1024 * 1024,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(doc.end.capture.extra["attempts"], 1);
            assert_eq!(doc.header.source.extra["capture_scope"]["mode"], "dump");
            let pids = sandbox.ready_pids();
            assert_eq!(pids.len(), 1);
            #[cfg(target_os = "linux")]
            ProcessObserver::open(pids[0]).assert_exited();
        }
    }
}

#[test]
fn native_executable_protocol_handles_paths_unicode_invalid_utf8_and_partial_eof() {
    let s = Sandbox::new("stream_edges");
    let devices = s.ok(&["devices", "--adb", s.adb.to_str().unwrap()]);
    assert!(devices.to_string().contains("TEST-USB"));
    let (ok, out, err) = s.run(&[
        "capture",
        "--serial",
        "TEST-USB",
        "--adb",
        s.adb.to_str().unwrap(),
        "--dump",
        "--duration-secs",
        "2",
    ]);
    assert!(ok, "{err}");
    let doc = ciallogcat::document::load(
        out.as_bytes(),
        4 * 1024 * 1024,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.header.schema_version, 3);
    assert_eq!(doc.entries.len(), 4);
    assert_eq!(doc.entries[0].kind, ciallogcat::model::LogEntryKind::Marker);
    assert_eq!(doc.entries[1].message.as_ref(), "unicode 雪 🚀");
    assert!(doc.entries[2].message.contains('\u{fffd}'));
    assert_eq!(doc.entries[3].message.as_ref(), "final partial");
    assert_eq!(doc.end.capture.extra["attempts"], 1);
    assert_eq!(
        serde_json::from_str::<Value>(err.trim()).unwrap()["reason"],
        "snapshot_complete"
    );
}

#[test]
fn native_background_stop_reaps_idle_child_and_publishes_finalized_snapshot() {
    let s = Sandbox::new("once_then_idle");
    let dir = s.path("session");
    s.ok(&[
        "start",
        "--serial",
        "TEST-USB",
        "--adb",
        s.adb.to_str().unwrap(),
        "--session-dir",
        &dir,
        "--duration-secs",
        "10",
    ]);
    let observers: Vec<_> = s
        .wait_ready()
        .into_iter()
        .map(ProcessObserver::open)
        .collect();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if s.ok(&["status", "--session-dir", &dir])["entries"]
            .as_u64()
            .unwrap()
            > 0
        {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    let query = s.ok(&["query", "--session-dir", &dir, "--mode", "tail"]);
    assert_eq!(query["records"][0]["message_preview"], "before idle");
    let stopped = s.ok(&["stop", "--session-dir", &dir]);
    assert_eq!(stopped["state"], "completed");
    assert_eq!(stopped["diagnostics_complete"], true);
    for observer in observers {
        observer.assert_exited();
    }
    let end = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        query["next_cursor"].as_str().unwrap(),
    ]);
    assert_eq!(end["artifact_finalized"], true);
}

#[test]
fn native_binary_pipe_preserves_protobuf_payload_and_can_be_stopped() {
    {
        let s = Sandbox::new("proto_normal");
        let dir = s.path("session");
        s.ok(&[
            "start",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb.to_str().unwrap(),
            "--session-dir",
            &dir,
            "--transport",
            "proto",
            "--duration-secs",
            "10",
        ]);
        let observers: Vec<_> = s
            .wait_ready()
            .into_iter()
            .map(ProcessObserver::open)
            .collect();
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if s.ok(&["status", "--session-dir", &dir])["entries"]
                .as_u64()
                .unwrap()
                > 0
            {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(20));
        }
        let page = s.ok(&[
            "query",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--start-at",
            "end",
        ]);
        assert_eq!(page["source_schema_version"], 3);
        assert_eq!(page["records"].as_array().unwrap().len(), 0);
        assert_eq!(page["validation"]["scope"], "tail_suffix");
        let stopped = s.ok(&["stop", "--session-dir", &dir]);
        assert_eq!(stopped["visible_end"]["offset"], stopped["bytes"]);
        let all = s.ok(&["query", "--session-dir", &dir]);
        assert_eq!(all["schema_version"], 2);
        assert_eq!(all["source_schema_version"], page["source_schema_version"]);
        assert_eq!(all["complete"], true);
        let detail = s.ok(&[
            "get",
            "--session-dir",
            &dir,
            "--ref",
            all["records"][0]["record_ref"].as_str().unwrap(),
            "--view",
            "decoded",
        ]);
        assert_eq!(
            detail["source_schema_version"],
            all["source_schema_version"]
        );
        let full: Value =
            serde_json::from_str(detail["record_json_chunk"].as_str().unwrap()).unwrap();
        assert_eq!(full["message"], "m\0\u{fffd}\n");
        for observer in observers {
            observer.assert_exited();
        }
        let bytes = fs::read(s.root.join("session/logs.jsonl")).unwrap();
        let doc = ciallogcat::document::load(
            std::io::Cursor::new(bytes),
            16 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(doc.entries.len(), 1);
        let entry = &doc.entries[0];
        assert_eq!(entry.message.as_ref(), "m\0\u{fffd}\n");
        assert_eq!(entry.tag.as_ref(), "Tag");
        assert_eq!(entry.level, ciallogcat::model::Level::Fatal);
        let proto = entry.proto.as_ref().expect("raw protobuf retained");
        assert_eq!(proto.raw_payload,b"\x08\x01\x10\x02\x18\x08\x20\x2a\x28\x04\x30\x05\x3a\x04Tag\x00\x42\x04m\x00\xff\n\x4a\x03app");
    }
}

#[test]
fn native_stderr_errors_and_proto_capability_failure_are_reported() {
    let s = Sandbox::new("stderr_exit");
    let (ok, _, err) = s.run(&[
        "capture",
        "--serial",
        "TEST-USB",
        "--adb",
        s.adb.to_str().unwrap(),
        "--duration-secs",
        "2",
    ]);
    assert!(!ok);
    assert!(err.contains("permission denied"), "{err}");
    let p = Sandbox::new("proto_no_shell_v2");
    let (ok, _, err) = p.run(&[
        "capture",
        "--serial",
        "TEST-USB",
        "--adb",
        p.adb.to_str().unwrap(),
        "--transport",
        "proto",
        "--duration-secs",
        "1",
    ]);
    assert!(!ok);
    assert!(err.contains("shell_v2"), "{err}");
    assert!(p.ready_pids().is_empty());
}

#[test]
fn native_burst_can_stop_with_bounded_output_and_complete_cleanup() {
    let s = Sandbox::new("burst");
    let dir = s.path("session");
    s.ok(&[
        "start",
        "--serial",
        "TEST-USB",
        "--adb",
        s.adb.to_str().unwrap(),
        "--session-dir",
        &dir,
        "--duration-secs",
        "10",
        "--max-bytes",
        "1048576",
    ]);
    // The writer's bound must stop a >32 MiB producer, including full pipes.
    let until = Instant::now() + Duration::from_secs(5);
    let done = loop {
        let status = s.ok(&["status", "--session-dir", &dir]);
        if status["state"] == "completed" {
            break status;
        }
        assert!(status["state"] != "failed", "{status}");
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(done["bytes"].as_u64().unwrap() <= 1048576);
    assert_eq!(done["omitted_by_limit"], true);
    assert_eq!(done["diagnostics_complete"], true);
    let bytes = fs::read(s.root.join("session/logs.jsonl")).unwrap();
    assert!(
        ciallogcat::document::load(
            std::io::Cursor::new(bytes),
            32 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false)
        )
        .is_ok()
    );
}
