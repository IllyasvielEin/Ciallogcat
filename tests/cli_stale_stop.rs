//! Stale workers are cooperatively requested to stop, never trusted by PID.
//! Administrative closure is an immutable frozen view, not a claim of exit.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ciallogcat::document::{DocumentHeader, StreamEncoder};
use ciallogcat::model::{Level, LogBuffer, LogEntry};
use ciallogcat::session::Session;
use serde_json::{Value, json};

#[path = "support/native_fixture.rs"]
mod native_fixture;

struct Sandbox(PathBuf);
struct Running {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
impl Running {
    fn finish(mut self) -> (bool, Value) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "CLI timed out");
            std::thread::sleep(Duration::from_millis(5));
        };
        let path = if status.success() {
            &self.stdout
        } else {
            &self.stderr
        };
        let bytes = fs::read(path).unwrap();
        (
            status.success(),
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes))),
        )
    }
}
impl Sandbox {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/cli-stale-stop")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn directory(&self) -> &str {
        self.0.to_str().unwrap()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn spawn(&self, args: &[&str]) -> Running {
        let id = uuid::Uuid::new_v4();
        let stdout = self.path(&format!("{id}.out"));
        let stderr = self.path(&format!("{id}.err"));
        let child = Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
            .args(args)
            .env("CIALLOGCAT_CONFIG_DIR", self.path("config"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        Running {
            child,
            stdout,
            stderr,
        }
    }
    fn run(&self, args: &[&str]) -> (bool, Value) {
        self.spawn(args).finish()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (ok, v) = self.run(args);
        assert!(ok, "{v}");
        v
    }
    fn status(&self, entries: u64, bytes: u64) -> Value {
        let mut status = json!({"schema_version":1,"type":"status","session":self.directory(),"worker_pid":std::process::id(),
        "state":"running","readiness":"adb_process_started","entries":entries,"bytes":bytes,"reason":null,"error":null,
        "updated_unix_ms":1,"stale":false,"omitted_by_limit":false,"diagnostic_lines":0,"last_diagnostic":null,
        "diagnostics_complete":false,"discarded_entries":0,"diagnostic_dropped_count":0});
        native_fixture::set_status(&mut status, entries, bytes);
        status
    }
    fn publish(&self, status: &Value) {
        let temporary = self.path(&format!("status-fixture-{}.tmp", uuid::Uuid::new_v4()));
        fs::write(&temporary, serde_json::to_vec(status).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match fs::rename(&temporary, self.path("status.json")) {
                Ok(()) => return,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "atomic fixture status publication: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    fn native_prefix(&self) -> (Value, Vec<u8>) {
        let one = [native_fixture::header_bytes(), line(1)].concat();
        let status = self.status(1, one.len() as u64);
        self.publish(&status);
        fs::write(
            self.path("logs.jsonl"),
            [one.clone(), line(2), b"{partial".to_vec()].concat(),
        )
        .unwrap();
        (status, one)
    }
    fn force(&self) -> Value {
        self.ok(&[
            "stop",
            "--session-dir",
            self.directory(),
            "--wait-secs",
            "1",
            "--force",
        ])
    }
    fn query(&self) -> Value {
        self.ok(&["query", "--session-dir", self.directory()])
    }
}
fn line(seq: u64) -> Vec<u8> {
    native_fixture::line(&native_fixture::row(seq, &format!("row-{seq}"), Some(1234)))
}

fn wait_marker(s: &Sandbox) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !s.path("stop").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn seqs(v: &Value) -> Vec<u64> {
    v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect()
}

#[test]
fn stale_stop_requests_cooperative_exit_then_times_out_without_pid_signal() {
    let s = Sandbox::new();
    let (status, _) = s.native_prefix();
    let before = fs::read(s.path("status.json")).unwrap();
    let (ok, result) = s.run(&["stop", "--session-dir", s.directory(), "--wait-secs", "1"]);
    assert!(!ok);
    assert_eq!(result["error"]["code"], "timeout");
    assert!(s.path("stop").is_file());
    assert_eq!(fs::read(s.path("status.json")).unwrap(), before);
    assert!(!s.path("management-closure.json").exists());
    assert_eq!(status["worker_pid"], std::process::id());
}

#[test]
fn stale_worker_can_acknowledge_normal_stop_and_force_preserves_real_completion() {
    for force in [false, true] {
        let s = Sandbox::new();
        let (mut status, _) = s.native_prefix();
        let mut args = vec!["stop", "--session-dir", s.directory(), "--wait-secs", "2"];
        if force {
            args.push("--force");
        }
        let running = s.spawn(&args);
        wait_marker(&s);
        status["state"] = "completed".into();
        status["reason"] = "stopped".into();
        s.publish(&status);
        let (ok, result) = running.finish();
        assert!(ok, "{result}");
        assert_eq!(result["state"], "completed");
        assert!(result.get("worker_exit_confirmed").is_none());
        assert!(!s.path("management-closure.json").exists());
    }
}

#[test]
fn forced_closure_freezes_published_native_prefix_and_preserves_worker_files() {
    let s = Sandbox::new();
    let (mut status, one) = s.native_prefix();
    fs::write(s.path("worker.lock"), b"stale lock is not liveness proof").unwrap();
    fs::write(s.path("status.next"), b"crashed unfinished publication").unwrap();
    let status_bytes = fs::read(s.path("status.json")).unwrap();
    let log_bytes = fs::read(s.path("logs.jsonl")).unwrap();
    let closed = s.force();
    assert_eq!(closed["state"], "abandoned");
    assert_eq!(closed["reason"], "administratively_closed");
    assert_eq!(closed["worker_exit_confirmed"], false);
    assert_eq!(closed["bytes"], one.len());
    assert_eq!(closed["stale"], false);
    assert_eq!(fs::read(s.path("status.json")).unwrap(), status_bytes);
    assert_eq!(fs::read(s.path("logs.jsonl")).unwrap(), log_bytes);
    assert_eq!(
        fs::read(s.path("status.next")).unwrap(),
        b"crashed unfinished publication"
    );
    assert_eq!(
        fs::read(s.path("worker.lock")).unwrap(),
        b"stale lock is not liveness proof"
    );
    let closure = fs::read(s.path("management-closure.json")).unwrap();
    assert_eq!(s.force(), closed);
    assert_eq!(
        fs::read(s.path("management-closure.json")).unwrap(),
        closure
    );
    // A delayed worker may write a newer status and more records. Its
    // files remain intact, while every management reader keeps the frozen view.
    let bytes = [one, line(2), line(3)].concat();
    fs::write(s.path("logs.jsonl"), &bytes).unwrap();
    status["entries"] = 3.into();
    status["visible_end"]["seq"] = 3.into();
    status["bytes"] = bytes.len().into();
    status["visible_end"]["offset"] = bytes.len().into();
    status["updated_unix_ms"] = u64::MAX.into();
    s.publish(&status);
    let shown = s.ok(&["status", "--session-dir", s.directory()]);
    assert_eq!(shown, closed);
    let page = s.query();
    assert_eq!(seqs(&page), [1]);
    assert_eq!(page["complete"], true);
    assert_eq!(page["source_status"]["worker_exit_confirmed"], false);
}

#[test]
fn concurrent_force_is_first_writer_wins_and_corrupt_sidecar_fails_closed() {
    let s = Sandbox::new();
    s.native_prefix();
    let args = [
        "stop",
        "--session-dir",
        s.directory(),
        "--wait-secs",
        "1",
        "--force",
    ];
    let a = s.spawn(&args);
    let b = s.spawn(&args);
    let (ok, a) = a.finish();
    assert!(ok, "{a}");
    let (ok, b) = b.finish();
    assert!(ok, "{b}");
    assert_eq!(a, b);
    assert!(!fs::read_dir(&s.0).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".management-closure-")
    }));
    fs::write(s.path("management-closure.json"), b"{truncated").unwrap();
    for command in ["status", "query", "stop"] {
        let (ok, v) = s.run(&[command, "--session-dir", s.directory()]);
        assert!(!ok, "{v}");
        assert_eq!(v["error"]["code"], "invalid_session");
    }
}

#[test]
fn old_cursors_beyond_closed_boundary_are_rejected_while_within_prefix_references_work() {
    let s = Sandbox::new();
    let one = [native_fixture::header_bytes(), line(1)].concat();
    let bytes = [one.clone(), line(2)].concat();
    fs::write(s.path("logs.jsonl"), &bytes).unwrap();
    let mut status = s.status(2, bytes.len() as u64);
    s.publish(&status);
    let old = s.ok(&["query", "--session-dir", s.directory(), "--limit", "1"]);
    status["entries"] = 1.into();
    status["visible_end"]["seq"] = 1.into();
    status["bytes"] = one.len().into();
    status["visible_end"]["offset"] = one.len().into();
    s.publish(&status);
    s.force();
    let token = old["next_cursor"].as_str().unwrap();
    let reference = old["records"][0]["record_ref"].as_str().unwrap();
    for args in [
        vec!["query", "--session-dir", s.directory(), "--cursor", token],
        vec!["get", "--session-dir", s.directory(), "--ref", reference],
        vec![
            "context",
            "--session-dir",
            s.directory(),
            "--ref",
            reference,
        ],
    ] {
        let (ok, v) = s.run(&args);
        assert!(!ok, "{v}");
        assert_eq!(v["error"]["code"], "stale_cursor");
    }
    let page = s.query();
    let reference = page["records"][0]["record_ref"].as_str().unwrap();
    assert_eq!(
        s.ok(&["get", "--session-dir", s.directory(), "--ref", reference])["complete"],
        true
    );
    assert_eq!(
        seqs(&s.ok(&[
            "context",
            "--session-dir",
            s.directory(),
            "--ref",
            reference
        ])),
        [1]
    );
}

#[test]
fn native_closed_sessions_remain_prefix_queries_without_fabricated_footer() {
    {
        let s = Sandbox::new();
        let mut session = Session::new();
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        let header = DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]);
        let mut encoder = StreamEncoder::new(header.clone()).unwrap();
        let mut bytes = encoder.header_bytes().unwrap();
        let mut entry = LogEntry::new("now", 42, 43, "Tag", Level::Info, "one");
        entry.provenance = Some(session.next_record(Some(1234)).unwrap());
        bytes.extend(encoder.record_bytes(&entry).unwrap());
        let published = bytes.len();
        entry.provenance = Some(session.next_record(Some(1235)).unwrap());
        bytes.extend(encoder.record_bytes(&entry).unwrap());
        fs::write(s.path("logs.jsonl"), &bytes).unwrap();
        let mut status = s.status(1, published as u64);
        status["record_schema_version"] = header.schema_version.into();
        status["artifact_id"] = header.artifact_id.to_string().into();
        status["session_id"] = header.session_id.unwrap().to_string().into();
        status["visible_end"] = json!({"seq":1,"offset":published});
        s.publish(&status);
        let closed = s.force();
        assert_eq!(closed["visible_end"], status["visible_end"]);
        let page = s.query();
        assert_eq!(seqs(&page), [1]);
        assert_eq!(page["artifact_finalized"], false);
        assert_eq!(page["source_status"]["state"], "abandoned");
        assert_eq!(page["source_status"]["terminal"], true);
        let reference = page["records"][0]["record_ref"].as_str().unwrap();
        assert_eq!(
            s.ok(&["get", "--session-dir", s.directory(), "--ref", reference])["complete"],
            true
        );
        assert_eq!(
            seqs(&s.ok(&[
                "context",
                "--session-dir",
                s.directory(),
                "--ref",
                reference,
                "--after",
                "10"
            ])),
            [1]
        );
        let (ok, v) = s.run(&["query", "--file", s.path("logs.jsonl").to_str().unwrap()]);
        assert!(!ok, "{v}");
        assert_eq!(fs::read(s.path("logs.jsonl")).unwrap(), bytes);
    }
}

#[test]
fn already_terminal_force_is_noop_and_frozen_partial_boundaries_fail_queries() {
    for state in ["completed", "failed"] {
        let s = Sandbox::new();
        let (mut status, _) = s.native_prefix();
        status["state"] = state.into();
        s.publish(&status);
        let before = fs::read(s.path("status.json")).unwrap();
        assert_eq!(s.force()["state"], state);
        assert!(!s.path("stop").exists());
        assert!(!s.path("management-closure.json").exists());
        assert_eq!(fs::read(s.path("status.json")).unwrap(), before);
    }
    let s = Sandbox::new();
    let (mut status, one) = s.native_prefix();
    status["bytes"] = (one.len() - 1).into();
    status["visible_end"]["offset"] = (one.len() - 1).into();
    s.publish(&status);
    s.force();
    let (ok, v) = s.run(&["query", "--session-dir", s.directory()]);
    assert!(!ok, "{v}");
}

#[test]
fn administrative_closure_enforces_sequence_fence_even_when_published_bytes_disagree() {
    {
        let s = Sandbox::new();
        let mut status = s.status(1, 0);
        let bytes = {
            let mut session = Session::new();
            session.begin_attempt().unwrap();
            session.stream_started().unwrap();
            let header = DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]);
            let mut encoder = StreamEncoder::new(header.clone()).unwrap();
            let mut bytes = encoder.header_bytes().unwrap();
            for index in 1..=3 {
                let mut entry = if index == 2 {
                    LogEntry::marker("structural event also has a sequence")
                } else {
                    LogEntry::new("now", 42, 43, "Tag", Level::Info, "one")
                };
                entry.provenance = Some(session.next_record(Some(1234 + index)).unwrap());
                bytes.extend(encoder.record_bytes(&entry).unwrap());
            }
            status["record_schema_version"] = header.schema_version.into();
            status["artifact_id"] = header.artifact_id.to_string().into();
            status["session_id"] = header.session_id.unwrap().to_string().into();
            status["visible_end"] = json!({"seq":1,"offset":bytes.len()});
            bytes
        };
        status["bytes"] = bytes.len().into();
        status["visible_end"]["offset"] = bytes.len().into();
        fs::write(s.path("logs.jsonl"), &bytes).unwrap();
        s.publish(&status);
        let page = s.ok(&["query", "--session-dir", s.directory(), "--limit", "1"]);
        let first = page["records"][0]["record_ref"].as_str().unwrap();
        // Context must reject records outside the signed sequence fence even
        // before administrative closure; a later closure cannot relax it.
        let (ok, error) = s.run(&[
            "context",
            "--session-dir",
            s.directory(),
            "--ref",
            first,
            "--after",
            "2",
        ]);
        assert!(!ok);
        assert_eq!(error["error"]["code"], "invalid_source");
        s.force();
        assert_eq!(
            s.ok(&["get", "--session-dir", s.directory(), "--ref", first])["seq"],
            1
        );
        for args in [
            vec!["query", "--session-dir", s.directory()],
            vec![
                "context",
                "--session-dir",
                s.directory(),
                "--ref",
                first,
                "--after",
                "2",
            ],
        ] {
            let (ok, value) = s.run(&args);
            assert!(!ok, "{value}");
            assert_eq!(value["error"]["code"], "invalid_source");
        }
        assert_eq!(fs::read(s.path("logs.jsonl")).unwrap(), bytes);
    }
}
