//! Actual executable coverage. Every command has a deadline and isolated state;
//! no command can discover a real Android device or use the developer's config.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

static UNIQUE: AtomicU64 = AtomicU64::new(0);
const WAIT: Duration = Duration::from_secs(15);

struct Sandbox {
    root: PathBuf,
    adb: PathBuf,
    scenario: String,
    trace: PathBuf,
}

impl Sandbox {
    fn new(scenario: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/cli-integration")
            .join(format!(
                "{scenario}-{}-{stamp}-{}",
                std::process::id(),
                UNIQUE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let adb = root.join("fake adb");
        // Some CLI scenarios intentionally replace this executable. Keep an
        // owned copy; sharing the library's immutable symlink would mutate its source.
        fs::write(&adb, include_bytes!("fixtures/fake_adb.py")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&adb, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            trace: root.join("adb-trace.jsonl"),
            root,
            adb,
            scenario: scenario.into(),
        }
    }

    fn run(&self, args: &[&str]) -> ResultOutput {
        let index = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let stdout = self.root.join(format!("command-{index}.stdout"));
        let stderr = self.root.join(format!("command-{index}.stderr"));
        let mut process = ManagedChild(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args(args)
                .env("CIALLOGCAT_ADB", &self.adb)
                .env("CIALLOGCAT_CONFIG_DIR", self.root.join("config"))
                .env("CIALLOGCAT_FAKE_SCENARIO", &self.scenario)
                .env("CIALLOGCAT_FAKE_TRACE", &self.trace)
                .stdin(Stdio::null())
                // File-backed streams are intentional: a detached worker must not
                // keep an inherited pipe open and hang Command::output forever.
                .stdout(fs::File::create(&stdout).unwrap())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI timed out: {args:?}; stderr: {}",
                fs::read_to_string(&stderr).unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        ResultOutput {
            status,
            stdout: fs::read_to_string(stdout).unwrap(),
            stderr: fs::read_to_string(stderr).unwrap(),
        }
    }

    #[cfg(target_os = "linux")]
    fn adb_path(&self) -> &str {
        self.adb.to_str().unwrap()
    }
    fn session(&self) -> PathBuf {
        self.root.join("session")
    }
    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(&self.trace)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn assert_reaped(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut pids: Vec<_> = self
                .trace()
                .iter()
                .filter_map(|row| row["pid"].as_u64())
                .collect();
            if let Ok(bytes) = fs::read(self.session().join("status.json"))
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                && let Some(pid) = value["worker_pid"].as_u64()
            {
                pids.push(pid);
            }
            let alive: Vec<_> = pids
                .into_iter()
                .filter(|pid| Path::new(&format!("/proc/{pid}")).exists())
                .collect();
            if alive.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "CLI/ADB subprocesses were not reaped: {alive:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Panic cleanup requests cooperative shutdown before the worker's own
        // capture bounds. Never kill a persisted PID from session metadata.
        if self.session().is_dir() {
            let _ = self.run(&[
                "stop",
                "--session-dir",
                self.session().to_str().unwrap(),
                "--wait-secs",
                "2",
            ]);
        }
        #[cfg(target_os = "linux")]
        for row in self.trace() {
            let Some(pid) = row["pid"].as_u64() else {
                continue;
            };
            if let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline"))
                && cmdline
                    .split(|byte| *byte == 0)
                    .any(|arg| arg == self.adb.as_os_str().as_encoded_bytes())
            {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
        // Preserve ignored traces and output for failure diagnosis.
    }
}

struct ManagedChild(Child);

#[cfg(target_os = "linux")]
#[test]
fn history_scope_rejects_bad_values_before_creating_output_or_starting_adb() {
    let sandbox = Sandbox::new("normal");
    let output = sandbox.root.join("must-not-exist.jsonl");
    for flags in [
        vec!["--record-format", "v3"],
        vec!["--record-format", "v3", "--transport", "text"],
        vec![
            "--record-format",
            "v3",
            "--transport",
            "proto",
            "--max-bytes",
            "8191",
        ],
        vec!["--record-format", "v1"],
        vec!["--record-format", "v2"],
        vec!["--max-bytes", "8191"],
        vec!["--tail", "0"],
        vec!["--tail", "+1"],
        vec!["--tail", "2147483648"],
        vec!["--tail", "1.0"],
        vec!["--tail", "1", "--since", "1.0"],
        vec!["--since", "1792742400"],
        vec!["--since", "1.0; touch injected"],
        vec!["--since", "$(id)"],
        vec!["--since", "1.0\n"],
        vec!["--since", "2025-02-29 00:00:00.0"],
        vec!["--since", "10-23 24:00:00.0"],
        vec!["--since", "1.1234567890"],
        vec!["--since", "--clear"],
        vec!["--buffers", "all,main"],
        vec!["--buffers", "main,main"],
        vec!["--buffers", "main;id"],
        vec!["--buffers", ""],
        vec!["--dump", "yes"],
        vec!["--dump", "--dump"],
        vec!["--duration-secs", "+1"],
    ] {
        let mut args = vec![
            "capture",
            "--serial",
            "TEST-USB",
            "--output",
            output.to_str().unwrap(),
        ];
        args.extend(flags);
        assert_eq!(
            sandbox.run(&args).error(2)["error"]["code"],
            "invalid_arguments"
        );
        assert!(!output.exists());
    }
    assert!(sandbox.trace().is_empty());
    let help = sandbox.run(&["capture", "--help"]);
    help.success();
    for flag in [
        "--tail",
        "--since",
        "--dump",
        "--buffers",
        "SECONDS.fraction",
        "-t N/TIME",
    ] {
        assert!(help.stdout.contains(flag));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn history_snapshot_selects_records_and_completes_once_with_actual_scope_in_header() {
    {
        for (flags, expected, scope_args) in [
            (
                vec![],
                vec!["history-1", "history-2", "history-3"],
                vec!["-d"],
            ),
            (
                vec!["--tail", "2"],
                vec!["history-2", "history-3"],
                vec!["-t", "2"],
            ),
            (
                vec!["--since", "10-23 12:00:02.000"],
                vec!["history-2", "history-3"],
                vec!["-t", "10-23 12:00:02.000"],
            ),
            (
                vec!["--since", "10-23 13:00:00.000"],
                vec![],
                vec!["-t", "10-23 13:00:00.000"],
            ),
        ] {
            let sandbox = Sandbox::new("history");
            let mut args = vec![
                "capture",
                "--serial",
                "TEST-USB",
                "--dump",
                "--buffers",
                "radio,events",
                "--duration-secs",
                "10",
            ];
            args.extend(flags);
            let started = Instant::now();
            let result = sandbox.run(&args);
            result.success();
            assert!(started.elapsed() < Duration::from_secs(3));
            let summary = result.summary();
            assert_eq!(summary["reason"], "snapshot_complete");
            assert_eq!(summary["entries"], expected.len());
            assert_eq!(summary["diagnostics_complete"], true);
            assert_eq!(summary["last_diagnostic"], "snapshot drained");
            let doc = ciallogcat::document::load(
                result.stdout.as_bytes(),
                4 * 1024 * 1024,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                doc.entries
                    .iter()
                    .map(|r| r.message.as_ref())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                doc.header.source.requested_buffers,
                Some(vec![
                    ciallogcat::model::LogBuffer::Events,
                    ciallogcat::model::LogBuffer::Radio
                ])
            );
            assert_eq!(doc.header.source.extra["capture_scope"]["mode"], "dump");
            assert_eq!(
                doc.header.source.extra["logcat_scope_args"],
                serde_json::json!(scope_args)
            );
            assert_eq!(doc.header.source.extra["host_budgets"]["duration_secs"], 10);
            assert_eq!(doc.end.capture.extra["attempts"], 1);
            assert!(doc.end.capture.known_gaps.is_empty());
            assert_eq!(summary["stream_interruptions"], 0);
            assert_eq!(summary["capture_completeness"], "unknown");
            let trace = sandbox.trace();
            assert_eq!(trace.len(), 1, "snapshot must not reconnect");
            let mut expected_args = vec!["-s", "TEST-USB", "logcat", "-v", "threadtime"];
            expected_args.extend(scope_args);
            expected_args.extend(["-b", "events", "-b", "radio"]);
            assert_eq!(trace[0]["args"], serde_json::json!(expected_args));
            sandbox.assert_reaped();
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn history_follow_keeps_live_stream_and_host_record_budget_after_selected_history() {
    for (flags, expected) in [
        (vec![], vec!["history-3", "live-after-history"]),
        (
            vec!["--tail", "2"],
            vec!["history-2", "history-3", "live-after-history"],
        ),
        (
            vec!["--since", "10-23 12:00:02.000"],
            vec!["history-2", "history-3", "live-after-history"],
        ),
    ] {
        let sandbox = Sandbox::new("history");
        let count = expected.len().to_string();
        let mut args = vec!["capture", "--serial", "TEST-USB", "--max-entries", &count];
        args.extend(flags);
        let output = sandbox.run(&args);
        output.success();
        let doc = ciallogcat::document::load(
            output.stdout.as_bytes(),
            4 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            doc.entries
                .iter()
                .map(|r| r.message.as_ref())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(doc.header.source.extra["capture_scope"]["mode"], "follow");
        assert_eq!(output.summary()["reason"], "max_entries");
        assert_eq!(output.summary()["stream_interruptions"], 0);
        assert_eq!(sandbox.trace()[0]["args"][5], "-T");
        sandbox.assert_reaped();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn history_proto_snapshot_preserves_calendar_word_and_empty_eof_is_success() {
    for (scenario, flags, expected_remote) in [
        (
            "proto_normal",
            vec!["--since", "2024-02-29 23:59:59.123456789"],
            "exec logcat --proto -v threadtime -t '2024-02-29 23:59:59.123456789' -b all '*:V'",
        ),
        (
            "proto_normal",
            vec!["--tail", "7"],
            "exec logcat --proto -v threadtime -t 7 -b all '*:V'",
        ),
        (
            "proto_zero_records",
            vec![],
            "exec logcat --proto -v threadtime -d -b all '*:V'",
        ),
        (
            "proto_normal",
            vec!["--since", "1792742400.0"],
            "exec logcat --proto -v threadtime -t 1792742400.0 -b all '*:V'",
        ),
    ] {
        let sandbox = Sandbox::new(scenario);
        let mut args = vec![
            "capture",
            "--serial",
            "TEST-USB",
            "--transport",
            "proto",
            "--dump",
            "--buffers",
            "all",
        ];
        args.extend(flags);
        let output = sandbox.run(&args);
        output.success();
        assert_eq!(output.summary()["reason"], "snapshot_complete");
        assert_eq!(output.summary()["stream_interruptions"], 0);
        let doc = ciallogcat::document::load(
            output.stdout.as_bytes(),
            4 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            doc.entries.len(),
            usize::from(scenario != "proto_zero_records")
        );
        assert_eq!(doc.end.capture.extra["attempts"], 1);
        assert!(doc.end.capture.known_gaps.is_empty());
        let trace = sandbox.trace();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[1]["args"][5], expected_remote);
        sandbox.assert_reaped();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn history_snapshot_errors_are_not_mislabeled_successful_eof() {
    for (scenario, transport) in [
        ("stderr_exit", "text"),
        ("proto_truncated", "proto"),
        ("eof_open", "text"),
    ] {
        let sandbox = Sandbox::new(scenario);
        let output = sandbox.run(&[
            "capture",
            "--serial",
            "TEST-USB",
            "--transport",
            transport,
            "--dump",
            "--duration-secs",
            "1",
        ]);
        assert_ne!(output.summary()["reason"], "snapshot_complete");
        assert_eq!(output.summary()["capture_completeness"], "partial");
        if scenario != "eof_open" {
            assert_eq!(output.status.code(), Some(4));
        }
        assert!(output.summary()["stream_interruptions"].as_u64().unwrap() > 0);
        sandbox.assert_reaped();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn history_snapshot_still_obeys_all_host_budgets() {
    for (scenario, budget, value, reason) in [
        ("history", "--max-entries", "1", "max_entries"),
        ("burst", "--max-bytes", "8192", "max_bytes"),
        ("once_then_idle", "--duration-secs", "1", "duration_limit"),
    ] {
        let sandbox = Sandbox::new(scenario);
        let output = sandbox.run(&["capture", "--serial", "TEST-USB", "--dump", budget, value]);
        output.success();
        assert_eq!(output.summary()["reason"], reason);
        if budget == "--max-bytes" {
            assert!(output.stdout.len() <= 8192);
        }
        if budget == "--max-entries" {
            assert_eq!(output.summary()["entries"], 1);
        }
        let doc = ciallogcat::document::load(
            output.stdout.as_bytes(),
            4 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(doc.end.capture.extra["attempts"], 1);
        sandbox.assert_reaped();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn history_background_snapshot_persists_scope_and_rejects_forged_worker_config() {
    let sandbox = Sandbox::new("history");
    let dir = sandbox.session();
    sandbox
        .run(&[
            "start",
            "--serial",
            "TEST-USB",
            "--session-dir",
            dir.to_str().unwrap(),
            "--dump",
            "--tail",
            "2",
            "--buffers",
            "radio",
        ])
        .success();
    let until = Instant::now() + Duration::from_secs(3);
    let status = loop {
        let status: Value =
            serde_json::from_slice(&fs::read(dir.join("status.json")).unwrap()).unwrap();
        if status["state"] == "completed" {
            break status;
        }
        assert!(Instant::now() < until, "{status}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status["reason"], "snapshot_complete");
    assert_eq!(status["entries"], 2);
    let config: Value =
        serde_json::from_slice(&fs::read(dir.join("config.json")).unwrap()).unwrap();
    // Default storage cap increased, but finite duration/entry bounds remain.
    assert_eq!(config["max_bytes"], 1_073_741_824u64);
    assert_eq!(config["duration_secs"], 600);
    assert_eq!(config["max_entries"], 10_000_000);
    assert_eq!(config["scope"]["mode"], "dump");
    assert_eq!(config["buffers"], serde_json::json!(["radio"]));
    for replacement in [
        serde_json::json!({"mode":"dump","start":{"kind":"since","value":"1.0;id"}}),
        serde_json::json!({"mode":"dump","start":{"kind":"tail","value":0}}),
    ] {
        let bad = Sandbox::new("history");
        fs::create_dir(bad.session()).unwrap();
        let mut config = config.clone();
        config["scope"] = replacement;
        fs::write(
            bad.session().join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert_eq!(
            bad.run(&["__worker", "--session-dir", bad.session().to_str().unwrap()])
                .error(3)["error"]["code"],
            "invalid_session"
        );
        assert!(bad.trace().is_empty());
        assert!(!bad.session().join("logs.jsonl").exists());
    }
    sandbox.assert_reaped();
}

#[cfg(target_os = "linux")]
#[test]
fn native_capture_reconnects_without_reusing_identity_and_finalizes_a_shared_document() {
    let sandbox = Sandbox::new("normal");
    let output = sandbox.run(&[
        "capture",
        "--adb",
        sandbox.adb_path(),
        "--serial",
        "TEST-USB",
        "--max-entries",
        "3",
        "--duration-secs",
        "5",
    ]);
    output.success();
    let doc = ciallogcat::document::load(
        std::io::Cursor::new(output.stdout.as_bytes()),
        64 * 1024 * 1024,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.entries.len(), 3);
    let ids: Vec<_> = doc.entries.iter().map(|e| e.provenance.unwrap()).collect();
    assert_eq!(ids.iter().map(|id| id.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(ids.iter().all(|id| id.session_id == ids[0].session_id));
    assert_eq!(
        ids.iter().map(|id| id.segment).collect::<Vec<_>>(),
        [1, 1, 2]
    );
    assert_eq!(doc.end.capture.completeness, "partial");
    assert_eq!(output.summary()["capture_completeness"], "partial");
    assert_eq!(output.summary()["stream_interruptions"], 1);
    assert!(
        doc.end.capture.extra["last_stream_error"]
            .as_str()
            .unwrap()
            .contains("exited")
    );
    assert_eq!(output.summary()["visible_end"]["seq"], 3);
    assert_eq!(
        output.summary()["visible_end"]["offset"],
        output.stdout.len()
    );
    assert_eq!(output.summary()["bytes"], output.stdout.len());
    sandbox.assert_reaped();
}

#[cfg(target_os = "linux")]
#[test]
fn native_footer_keeps_escaped_diagnostics_inside_the_reserved_budget() {
    {
        for serial in ["escaped", "ascii"] {
            let sandbox = Sandbox::new("normal");
            fs::write(&sandbox.adb, r#"#!/usr/bin/env python3
import json, os, pathlib, struct, sys
args = sys.argv[1:]
with open(os.environ['CIALLOGCAT_FAKE_TRACE'], 'a') as trace:
    trace.write(json.dumps({'pid':os.getpid(),'args':args,'event':'invocation'})+'\n')
state = pathlib.Path(__file__ + '.state')
if len(args) == 3 and args[0] == '-s' and args[2] == 'features':
    if not state.exists():
        state.write_text('first capability query passed')
        print('shell_v2')
        raise SystemExit(0)
    os.write(2, b'capability error: ' + (b'x' if args[1] == 'ascii' else b'\x01') * 1000)
    raise SystemExit(17)
if len(args) == 6 and args[2:5] == ['shell','-T','-n'] and args[5].startswith('exec logcat --proto '):
    tag, msg = b'Review', b'valid evidence before disconnect'
    payload = b'\x08\x01\x10\x00\x18\x04\x20\x01\x28\x01\x30\x01' + bytes([0x3a,len(tag)]) + tag + bytes([0x42,len(msg)]) + msg
    os.write(1, struct.pack('<Q',len(payload))+payload)
    raise SystemExit(0)
raise SystemExit(2)
"#).unwrap();
            let file = sandbox.root.join("capture.jsonl");
            let output = sandbox.run(&[
                "capture",
                "--adb",
                sandbox.adb_path(),
                "--serial",
                serial,
                "--transport",
                "proto",
                "--duration-secs",
                "2",
                "--max-bytes",
                "8192",
                "--output",
                file.to_str().unwrap(),
            ]);
            output.success();
            let bytes = fs::read(&file).unwrap();
            assert!(bytes.len() <= 8192);
            let document = ciallogcat::document::load(
                std::io::Cursor::new(&bytes),
                64 * 1024 * 1024,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(document.entries.len(), 1);
            assert_eq!(
                document.entries[0].message.as_ref(),
                "valid evidence before disconnect"
            );
            assert_eq!(document.end.capture.completeness, "partial");
            let diagnostic = &document.end.capture.extra["last_stream_error"];
            assert!(serde_json::to_vec(diagnostic).unwrap().len() <= 1024);
            if serial == "escaped" {
                assert_eq!(
                    document.end.capture.extra["last_stream_error_truncated"],
                    true
                );
                assert!(diagnostic.as_str().unwrap().contains('\u{1}'));
            }
            assert_eq!(output.summary()["visible_end"]["seq"], 1);
            assert_eq!(output.summary()["visible_end"]["offset"], bytes.len());
            assert_eq!(
                sandbox
                    .run(&["query", "--file", file.to_str().unwrap()])
                    .success()
                    .json()["artifact_finalized"],
                true
            );
            sandbox.assert_reaped();
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_capture_size_budget_includes_metadata_and_publishes_only_complete_records() {
    let sandbox = Sandbox::new("burst");
    let output = sandbox.run(&[
        "capture",
        "--adb",
        sandbox.adb_path(),
        "--serial",
        "TEST-USB",
        "--max-bytes",
        "8192",
        "--duration-secs",
        "2",
    ]);
    output.success();
    let doc = ciallogcat::document::load(
        std::io::Cursor::new(output.stdout.as_bytes()),
        64 * 1024 * 1024,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.entries.len(), 0);
    assert!(output.stdout.len() <= 8192);
    assert_eq!(output.summary()["reason"], "max_bytes");
    assert_eq!(output.summary()["visible_end"]["seq"], 0);
    assert!(output.summary()["discarded_entries"].as_u64().unwrap() > 0);
    assert_eq!(doc.end.capture.completeness, "partial");
    sandbox.assert_reaped();
}

#[cfg(target_os = "linux")]
#[test]
fn native_spawn_failure_cannot_be_reported_as_success_when_duration_expires() {
    let sandbox = Sandbox::new("normal");
    fs::write(&sandbox.adb, b"not an executable").unwrap();
    let output = sandbox.run(&[
        "capture",
        "--adb",
        sandbox.adb_path(),
        "--serial",
        "TEST-USB",
        "--duration-secs",
        "1",
    ]);
    assert_eq!(output.status.code(), Some(4), "{}", output.stderr);
    assert_eq!(output.summary()["state"], "failed");
    assert!(output.summary()["readiness"].is_null());
    let doc = ciallogcat::document::load(
        std::io::Cursor::new(output.stdout.as_bytes()),
        64 * 1024 * 1024,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.end.capture.state, "failed");
}

#[cfg(target_os = "linux")]
#[test]
fn native_background_capture_queries_and_stops_with_a_finalized_boundary() {
    let sandbox = Sandbox::new("continuous");
    let dir = sandbox.session();
    let dir = dir.to_str().unwrap();
    let started = sandbox.run(&[
        "start",
        "--adb",
        sandbox.adb_path(),
        "--serial",
        "TEST-USB",
        "--session-dir",
        dir,
        "--duration-secs",
        "10",
    ]);
    started.success();
    assert_eq!(started.json()["record_schema_version"], 3);
    // Readiness confirms spawn, while records become visible on the next
    // published writer boundary; an empty first query is legitimate.
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let status = sandbox.run(&["status", "--session-dir", dir]);
        status.success();
        if status.json()["entries"].as_u64().unwrap() > 0 {
            break;
        }
        assert!(Instant::now() < until, "no visible records");
        std::thread::sleep(Duration::from_millis(20));
    }
    let queried = sandbox.run(&["query", "--session-dir", dir, "--mode", "tail"]);
    queried.success();
    assert!(!queried.json()["records"].as_array().unwrap().is_empty());
    let stopped = sandbox.run(&["stop", "--session-dir", dir]);
    stopped.success();
    let bytes = fs::read(Path::new(dir).join("logs.jsonl")).unwrap();
    let doc = ciallogcat::document::load(
        std::io::Cursor::new(&bytes),
        64 * 1024 * 1024,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(stopped.json()["visible_end"]["offset"], bytes.len());
    assert_eq!(stopped.json()["entries"], doc.entries.len());
    assert_eq!(stopped.json()["reason"], "stopped");
    sandbox.assert_reaped();
}

#[cfg(target_os = "linux")]
#[test]
fn native_quiet_writer_failure_is_observed_and_reaps_the_owned_child() {
    let sandbox = Sandbox::new("once_then_idle");
    // A pipe whose only reader closes forces a real broken pipe without
    // depending on another device record to notify the capture loop.
    let mut process = ManagedChild(
        Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
            .args([
                "capture",
                "--adb",
                sandbox.adb_path(),
                "--serial",
                "TEST-USB",
                "--duration-secs",
                "10",
            ])
            .env("CIALLOGCAT_FAKE_SCENARIO", "once_then_idle")
            .env("CIALLOGCAT_FAKE_TRACE", &sandbox.trace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    drop(process.0.stdout.take());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(5));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "writer failure ignored until capture deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    use std::io::Read;
    let mut stderr = String::new();
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.to_lowercase().contains("broken pipe"), "{stderr}");
    sandbox.assert_reaped();
}

#[cfg(target_os = "linux")]
#[test]
fn experimental_protobuf_capture_round_trips_evidence_and_rejects_unsupported_streams() {
    {
        let sandbox = Sandbox::new("proto_normal");
        let output = sandbox.run(&[
            "capture",
            "--adb",
            sandbox.adb_path(),
            "--serial",
            "TEST-USB",
            "--transport",
            "proto",
            "--max-entries",
            "1",
            "--duration-secs",
            "2",
        ]);
        output.success();
        let doc = ciallogcat::document::load(
            std::io::Cursor::new(output.stdout.as_bytes()),
            64 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            doc.header.source.transport_format.as_deref(),
            Some("logcat_proto_experimental")
        );
        assert_eq!(doc.entries.len(), 1);
        let evidence = doc.entries[0].proto.as_ref().unwrap();
        assert_eq!(evidence.priority, 8);
        assert_eq!(doc.entries[0].level, ciallogcat::model::Level::Fatal);
        assert!(!evidence.raw_payload.is_empty());
        sandbox.assert_reaped();
        for scenario in ["proto_malformed", "proto_unsupported", "proto_no_shell_v2"] {
            let sandbox = Sandbox::new(scenario);
            let output = sandbox.run(&[
                "capture",
                "--adb",
                sandbox.adb_path(),
                "--serial",
                "TEST-USB",
                "--transport",
                "proto",
                "--duration-secs",
                "1",
            ]);
            assert_eq!(
                output.status.code(),
                Some(4),
                "{scenario}: {}",
                output.stderr
            );
            assert_eq!(output.summary()["state"], "failed");
            assert!(
                !sandbox.trace().iter().any(|row| row["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "logcat")),
                "text fallback attempted"
            );
            sandbox.assert_reaped();
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_protobuf_stop_reports_incomplete_next_frame_as_an_explicit_gap() {
    {
        let sandbox = Sandbox::new("proto_partial_then_idle");
        let output = sandbox.run(&[
            "capture",
            "--adb",
            sandbox.adb_path(),
            "--serial",
            "TEST-USB",
            "--transport",
            "proto",
            "--duration-secs",
            "1",
        ]);
        output.success();
        let doc = ciallogcat::document::load(
            std::io::Cursor::new(output.stdout.as_bytes()),
            64 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(doc.entries.len(), 1);
        assert_eq!(doc.end.capture.completeness, "partial");
        assert!(
            doc.end
                .capture
                .known_gaps
                .iter()
                .any(|gap| gap["kind"] == "incomplete_frame_at_stop")
        );
        assert_eq!(output.summary()["state"], "completed");
        assert_eq!(output.summary()["diagnostics_complete"], true);
        sandbox.assert_reaped();
    }
}

#[test]
fn worker_pre_capture_failure_publishes_terminal_status_but_a_duplicate_cannot() {
    let sandbox = Sandbox::new("normal");
    let dir = sandbox.session();
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("config.json"), b"not-json").unwrap();
    let result = sandbox.run(&["__worker", "--session-dir", dir.to_str().unwrap()]);
    assert!(!result.status.success());
    let initial = fs::read(dir.join("status.json")).unwrap();
    let status: Value = serde_json::from_slice(&initial).unwrap();
    assert_eq!(status["state"], "failed");
    assert!(status["error"].as_str().unwrap().contains("expected"));
    let duplicate = sandbox.run(&["__worker", "--session-dir", dir.to_str().unwrap()]);
    assert!(!duplicate.status.success());
    assert_eq!(initial, fs::read(dir.join("status.json")).unwrap());
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct ResultOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}
impl ResultOutput {
    fn success(&self) -> &Self {
        assert!(
            self.status.success(),
            "status={} stdout={} stderr={}",
            self.status,
            self.stdout,
            self.stderr
        );
        self
    }
    #[cfg(target_os = "linux")]
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("invalid stdout JSON: {e}: {}", self.stdout))
    }
    #[cfg(target_os = "linux")]
    fn summary(&self) -> Value {
        self.stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == "status")
            .expect("missing capture status summary")
    }
    fn error(&self, code: i32) -> Value {
        assert_eq!(
            self.status.code(),
            Some(code),
            "stdout={} stderr={}",
            self.stdout,
            self.stderr
        );
        assert!(
            self.stdout.is_empty(),
            "errors must not pollute the data stream"
        );
        let messages: Vec<Value> = self
            .stderr
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let value = messages.last().expect("missing JSON error").clone();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["type"], "error");
        assert!(value["error"]["code"].is_string());
        assert!(value["error"]["message"].is_string());
        value
    }
}

#[test]
fn help_and_version_work_without_adb() {
    let s = Sandbox::new("normal");
    let help = s.run(&["--help"]);
    help.success();
    for command in [
        "devices", "capture", "start", "status", "query", "get", "context", "stop",
    ] {
        assert!(help.stdout.contains(command));
    }
    assert!(!help.stdout.contains("query-v2"));
    assert!(!help.stdout.contains("--after-seq"));
    assert!(!help.stdout.contains("--tag-include"));
    assert!(!help.stdout.contains("get-v2"));
    assert!(!help.stdout.contains("context-v2"));
    assert!(!help.stdout.contains("--record-format"));
    assert!(help.stdout.contains("--transport text|proto"));
    let version = s.run(&["--version"]);
    version.success();
    assert!(version.stdout.contains(env!("CARGO_PKG_VERSION")));
    assert!(s.trace().is_empty());
}

#[test]
fn query_help_exposes_budgets_and_continuations_without_opening_a_source() {
    let s = Sandbox::new("normal");
    for command in [None, Some("query"), Some("get"), Some("context")] {
        for flag in ["--help", "-h"] {
            let args = command.map_or_else(|| vec![flag], |command| vec![command, flag]);
            let help = s.run(&args);
            help.success();
            assert!(help.stderr.is_empty());
            assert!(!help.stdout.contains("query-v2"));
            assert!(!help.stdout.contains("--after-seq"));
            for expected in [
                "--file FILE",
                "--session-dir DIR",
                "--max-response-bytes N  Default 65536; range 8192..=1048576",
            ] {
                assert!(
                    help.stdout.contains(expected),
                    "{args:?}: missing {expected}"
                );
            }
            if command == Some("get") {
                assert!(!help.stdout.contains("--preview-bytes"));
                assert!(!help.stdout.contains("--max-scan-bytes"));
                assert!(!help.stdout.contains("--max-scan-records"));
            } else {
                for expected in [
                    "--preview-bytes N     Default 2048; range 0..=16384",
                    "--max-scan-bytes N    Default 16777216; range 8388609..=67108864",
                    "--max-scan-records N  Default 100000; range 1..=1000000",
                    "rejected by get",
                    "not total file I/O",
                ] {
                    assert!(
                        help.stdout.contains(expected),
                        "{args:?}: missing {expected}"
                    );
                }
            }
            let guidance: &[&str] = match command {
                Some("query") => &[
                    "--limit N",
                    "1..=1000",
                    "--query TEXT",
                    "4096 UTF-8 bytes",
                    "--mode snapshot|tail",
                    "--cursor TOKEN",
                    "--start-at beginning|end",
                    "prefix is not scanned",
                    "next_cursor",
                    "has_more",
                    "even if records\nis empty",
                    "scan.stop_reason",
                ],
                Some("get") => &[
                    "--ref TOKEN",
                    "--text-offset N",
                    "--view stored|decoded",
                    "derived view",
                    "0..=67108864",
                    "0..=8388608",
                    "record_json_chunk",
                    "next_text_offset",
                    "complete=true",
                    "preview/scan budget flags are rejected",
                ],
                Some("context") => &[
                    "--ref TOKEN",
                    "--before N",
                    "--after N",
                    "0..=100",
                    "including the target",
                    "partial_before/partial_after",
                    "no context cursor",
                ],
                _ => &["query --help", "get --help", "context --help"],
            };
            for expected in guidance {
                assert!(
                    help.stdout.contains(expected),
                    "{args:?}: missing {expected}"
                );
            }
        }
    }
    let focused = s.run(&["query", "--help"]);
    focused.success();
    assert!(
        focused
            .stdout
            .starts_with("query (--file FILE | --session-dir DIR)")
    );
    assert!(!focused.stdout.contains("Commands:"));
    // Help must not open paths or create the signing key just to describe usage.
    s.run(&["query", "--file", "missing.jsonl", "--help"])
        .success();
    assert!(!s.root.join("config").exists());
    assert!(s.trace().is_empty());
}

#[test]
fn query_reads_native_sources_and_rejects_invalid_or_unsupported_records() {
    use ciallogcat::document::{CaptureStatus, DocumentHeader, StreamEncoder};
    use ciallogcat::model::{Level, LogBuffer, LogEntry};
    use ciallogcat::session::Session;

    for proto in [false, true] {
        let schema = 3;
        let s = Sandbox::new("normal");
        let dir = s.session();
        fs::create_dir(&dir).unwrap();
        let logs = dir.join("logs.jsonl");
        let mut session = Session::new();
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        let mut entry = if proto {
            ciallogcat::protobuf::DecodedEntry::decode_payload(
                b"\x08\x01\x10\x02\x18\x04\x28\x2a\x30\x2b\x42\x03one".to_vec(),
            )
            .unwrap()
            .project()
        } else {
            LogEntry::new(
                "10-23 12:34:56.789",
                42,
                43,
                "Test",
                Level::Error,
                r#"message containing {"schema_version":2,"type":"document"}"#,
            )
        };
        entry.provenance = session.next_record(Some(1));
        let mut status = serde_json::json!({
            "schema_version":1,"type":"status","session":dir,"worker_pid":0,
            "state":"completed","readiness":null,"entries":1,"bytes":0,
            "reason":"max_entries","error":null,"updated_unix_ms":0,"stale":false,
            "omitted_by_limit":false,"diagnostic_lines":0,"last_diagnostic":null,
            "diagnostics_complete":true,"diagnostic_dropped_count":0,"discarded_entries":0
        });
        let header = DocumentHeader::live(session.id(), "TEST-USB".into(), vec![LogBuffer::Main]);
        let mut encoder = StreamEncoder::new(header.clone()).unwrap();
        let mut bytes = encoder.header_bytes().unwrap();
        bytes.extend(encoder.record_bytes(&entry).unwrap());
        bytes.extend(encoder.finish_bytes(CaptureStatus::default()).unwrap());
        status["record_schema_version"] = schema.into();
        status["artifact_id"] = header.artifact_id.to_string().into();
        status["session_id"] = session.id().to_string().into();
        status["visible_end"] = serde_json::json!({"seq":1,"offset":bytes.len()});
        status["bytes"] = bytes.len().into();
        fs::write(&logs, &bytes).unwrap();
        fs::write(
            dir.join("status.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        for args in [
            vec!["query", "--session-dir", dir.to_str().unwrap()],
            vec!["query", "--file", logs.to_str().unwrap()],
        ] {
            let result = s.run(&args);
            result.success();
            let value: Value = serde_json::from_str(&result.stdout).unwrap();
            assert_eq!(value["schema_version"], 2);
            assert_eq!(value["source_schema_version"], schema);
            assert_eq!(value["records"].as_array().unwrap().len(), 1);
            assert_eq!(value["records"][0]["seq"], 1);
            assert_eq!(
                value["records"][0]["message_preview"],
                entry.message.as_ref()
            );
            assert!(value["records"][0]["record_ref"].is_string());
            assert!(value["next_cursor"].is_string());
            assert_eq!(value["complete"], true);
            assert!(value.get("entries").is_none());
            assert!(value.get("next_after_seq").is_none());
        }

        // Unsupported v1 records and fake native headers must fail rather than
        // being reinterpreted as a different format or silently skipped.
        for (malformed, code) in [
            (
                b"{\"schema_version\":1,\"type\":\"entry\"}\n".as_slice(),
                "unsupported_format",
            ),
            (
                b"{\"schema_version\":2,\"type\":\"document\"}\n".as_slice(),
                "unsupported_format",
            ),
            (
                b"{\"schema_version\":3,\"type\":\"document\"}\n".as_slice(),
                "invalid_source",
            ),
        ] {
            fs::write(&logs, malformed).unwrap();
            status["bytes"] = malformed.len().into();
            status["visible_end"]["offset"] = malformed.len().into();
            fs::write(
                dir.join("status.json"),
                serde_json::to_vec(&status).unwrap(),
            )
            .unwrap();
            for args in [
                vec!["query", "--session-dir", dir.to_str().unwrap()],
                vec!["query", "--file", logs.to_str().unwrap()],
            ] {
                let error = s.run(&args).error(3);
                assert_eq!(error["error"]["code"], code);
            }
        }
        assert!(s.trace().is_empty());
    }
}

#[test]
fn query_rejects_v1_files_and_sessions() {
    let s = Sandbox::new("normal");
    let dir = s.session();
    fs::create_dir(&dir).unwrap();
    let logs = dir.join("logs.jsonl");
    let bytes = b"{\"schema_version\":1,\"type\":\"entry\",\"seq\":1,\"time\":\"now\",\"pid\":1,\"tid\":1,\"level\":\"I\",\"tag\":\"T\",\"message\":\"old record\"}\n";
    fs::write(&logs, bytes).unwrap();
    let mut status = serde_json::json!({
        "schema_version":1,"type":"status","session":dir,"worker_pid":0,
        "state":"completed","readiness":null,"entries":1,"bytes":bytes.len(),
        "reason":"max_entries","error":null,"updated_unix_ms":0,"stale":false,
        "omitted_by_limit":false,"diagnostic_lines":0,"last_diagnostic":null,
        "diagnostics_complete":true,"diagnostic_dropped_count":0,"discarded_entries":0,
        "record_schema_version":1
    });
    for explicit_schema in [true, false] {
        if !explicit_schema {
            status
                .as_object_mut()
                .unwrap()
                .remove("record_schema_version");
        }
        fs::write(
            dir.join("status.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        let error = s
            .run(&["query", "--session-dir", dir.to_str().unwrap()])
            .error(3);
        assert_eq!(error["error"]["code"], "unsupported_format");
    }
    let error = s.run(&["query", "--file", logs.to_str().unwrap()]).error(3);
    assert_eq!(error["error"]["code"], "unsupported_format");
    assert_eq!(fs::read(&logs).unwrap(), bytes);
    assert!(s.trace().is_empty());
}

#[test]
fn removed_query_command_and_sequence_pagination_are_rejected() {
    let s = Sandbox::new("normal");
    for args in [
        vec!["query-v2"],
        vec!["query-v2", "--help"],
        vec!["query-v2", "--file", "missing.jsonl"],
        vec!["get-v2", "--help"],
        vec!["context-v2", "--help"],
        vec!["query", "--session-dir", "missing", "--after-seq", "0"],
        vec!["query", "--session-dir", "missing", "--limit", "1001"],
    ] {
        let error = s.run(&args).error(2);
        assert_eq!(error["error"]["code"], "invalid_arguments");
    }
    assert!(s.trace().is_empty());
}

#[test]
fn native_cli_never_turns_raw_magic_key_objects_into_numbers() {
    use ciallogcat::document::{CaptureStatus, DocumentHeader, StreamEncoder};
    use ciallogcat::model::{Level, LogBuffer, LogEntry};
    use ciallogcat::session::Session;
    let s = Sandbox::new("normal");
    let dir = s.session();
    fs::create_dir(&dir).unwrap();
    let logs = dir.join("logs.jsonl");
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let header = DocumentHeader::live(session.id(), "TEST-USB".into(), vec![LogBuffer::Main]);
    let mut encoder = StreamEncoder::new(header.clone()).unwrap();
    let mut row = LogEntry::new(
        "now",
        1,
        1,
        "T",
        Level::Info,
        "literal $serde_json::private::Number",
    );
    row.provenance = session.next_record(Some(1));
    let header_bytes = encoder.header_bytes().unwrap();
    let record = String::from_utf8(encoder.record_bytes(&row).unwrap()).unwrap();
    let footer = encoder.finish_bytes(CaptureStatus::default()).unwrap();
    let mut status = serde_json::json!({
        "schema_version":1,"type":"status","session":dir,"worker_pid":0,
        "state":"completed","readiness":null,"entries":1,"bytes":0,
        "reason":"max_entries","error":null,"updated_unix_ms":0,"stale":false,
        "omitted_by_limit":false,"diagnostic_lines":0,"last_diagnostic":null,
        "diagnostics_complete":true,"diagnostic_dropped_count":0,"discarded_entries":0,
        "record_schema_version":3,"artifact_id":header.artifact_id,"session_id":session.id(),
        "visible_end":{"seq":1,"offset":0}
    });
    for raw in [
        r#"{"$serde_json::private::Number":"1e-400"}"#,
        r#"{"nested":[{"\u0024serde_json::private::Number":"18446744073709551617"}]}"#,
    ] {
        let corrupt = format!(
            "{},\"future\":{raw}}}\n",
            record.trim_end().strip_suffix('}').unwrap()
        );
        let bytes = [
            header_bytes.as_slice(),
            corrupt.as_bytes(),
            footer.as_slice(),
        ]
        .concat();
        fs::write(&logs, &bytes).unwrap();
        status["bytes"] = bytes.len().into();
        status["visible_end"]["offset"] = bytes.len().into();
        fs::write(
            dir.join("status.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        for args in [
            vec!["query", "--session-dir", dir.to_str().unwrap()],
            vec!["query", "--file", logs.to_str().unwrap()],
        ] {
            let error = s.run(&args).error(3);
            assert_eq!(error["error"]["code"], "invalid_source");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("unsupported JSON object key")
            );
        }
    }
    let bytes = [
        header_bytes.as_slice(),
        record.as_bytes(),
        footer.as_slice(),
    ]
    .concat();
    fs::write(&logs, &bytes).unwrap();
    s.run(&["query", "--file", logs.to_str().unwrap()])
        .success();
    status["bytes"] = bytes.len().into();
    status["visible_end"]["offset"] = bytes.len().into();
    status["entries"] = serde_json::json!({"$serde_json::private::Number":"1"});
    fs::write(
        dir.join("status.json"),
        serde_json::to_vec(&status).unwrap(),
    )
    .unwrap();
    let error = s
        .run(&["query", "--session-dir", dir.to_str().unwrap()])
        .error(3);
    assert_eq!(error["error"]["code"], "invalid_source");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported JSON object key")
    );
    assert!(s.trace().is_empty());
}

#[test]
fn invalid_arguments_fail_before_launching_adb() {
    let s = Sandbox::new("normal");
    for args in [
        vec!["does-not-exist"],
        vec!["capture"],
        vec!["capture", "--serial", "TEST-USB", "--duration-secs", "0"],
        vec!["capture", "--serial", "TEST-USB", "--max-entries", "0"],
        vec!["capture", "--serial", "TEST-USB", "--max-entries", "-1"],
        vec!["capture", "--serial", "TEST-USB", "--serial", "TEST-USB"],
        vec!["capture", "--serial", "TEST-USB", "--max-bytes", "0"],
        vec![
            "capture",
            "--serial",
            "TEST-USB",
            "--duration-secs",
            "86401",
        ],
        vec![
            "capture",
            "--serial",
            "TEST-USB",
            "--max-bytes",
            "18446744073709551616",
        ],
        vec!["query", "--session-dir", "missing", "--limit", "10001"],
        vec![
            "start",
            "--serial",
            "TEST-USB",
            "--session-dir",
            "missing",
            "--bogus",
        ],
    ] {
        s.run(&args).error(2);
    }
    assert!(s.trace().is_empty(), "invalid input spawned ADB");
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    fn entries(text: &str) -> Vec<Value> {
        // Validate the complete native document, including its footer, before
        // inspecting raw record fields; metadata must not count as log entries.
        let doc = ciallogcat::document::load(
            text.as_bytes(),
            64 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(doc.header.schema_version, 3);
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| matches!(row["type"].as_str(), Some("entry" | "event" | "unparsed")))
            .collect();
        assert_eq!(records.len(), doc.entries.len());
        for row in &records {
            assert_eq!(row["schema_version"], 3);
            assert!(row["seq"].is_u64());
            assert!(row["message"].is_string());
            assert_eq!(row["provenance"]["seq"], row["seq"]);
            assert_eq!(
                row["provenance"]["session_id"],
                serde_json::json!(doc.header.session_id)
            );
            if row["type"] == "entry" {
                for key in ["level", "tag"] {
                    assert!(row[key].is_string(), "missing {key}: {row}");
                }
                assert!(row["device_time"]["text"].is_string());
                assert!(row["pid"].is_u64());
                assert!(row["tid"].is_u64());
            } else {
                for key in ["pid", "tid", "level", "tag"] {
                    assert!(row[key].is_null(), "fabricated {key}: {row}");
                }
            }
        }
        records
    }

    #[test]
    fn devices_empty_is_success_but_adb_failure_is_error() {
        for scenario in ["normal", "no_devices", "query_error"] {
            let s = Sandbox::new(scenario);
            let out = s.run(&["devices", "--adb", s.adb_path()]);
            if scenario == "query_error" {
                out.error(4);
            } else {
                let value = out.success().json();
                assert_eq!(value["type"], "devices");
                let devices = value["devices"].as_array().unwrap();
                assert_eq!(devices.len(), usize::from(scenario == "normal"));
            }
            s.assert_reaped();
        }
    }

    #[test]
    fn capture_stdout_jsonl_and_explicit_device_are_exact() {
        let s = Sandbox::new("multi_device");
        let unix_ms = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        let before = unix_ms();
        let out = s.run(&[
            "capture",
            "--serial",
            "emulator-5554",
            "--adb",
            s.adb_path(),
            "--max-entries",
            "2",
        ]);
        let after = unix_ms();
        let rows = entries(&out.success().stdout);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let observed = row["provenance"]["observed_unix_ms"].as_u64().unwrap();
            assert!((before..=after).contains(&observed));
            assert!(row["provenance"]["session_id"].is_string());
            assert_eq!(row["provenance"]["attempt"], 1);
            assert_eq!(row["provenance"]["segment"], 1);
        }
        let summary = out.summary();
        assert_eq!(summary["entries"], rows.len());
        assert_eq!(summary["bytes"], out.stdout.len());
        assert_eq!(summary["record_schema_version"], 3);
        assert_eq!(summary["visible_end"]["seq"], 2);
        assert_eq!(summary["visible_end"]["offset"], out.stdout.len());
        assert_eq!(rows[0]["type"], "event");
        assert_eq!(rows[1]["message"], "hello from fake adb");
        assert_eq!(rows[1]["level"], "I");
        assert_eq!(rows[1]["pid"], 1234);
        assert!(rows[0]["seq"].as_u64().unwrap() < rows[1]["seq"].as_u64().unwrap());
        let streams: Vec<_> = s
            .trace()
            .into_iter()
            .filter(|row| {
                row["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "logcat")
            })
            .collect();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["args"][1], "emulator-5554");
        s.assert_reaped();
    }

    #[test]
    fn capture_file_never_overwrites_existing_output() {
        let s = Sandbox::new("continuous");
        let output = s.root.join("capture.jsonl");
        fs::write(&output, "keep this data\n").unwrap();
        s.run(&[
            "capture",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--output",
            output.to_str().unwrap(),
        ])
        .error(5);
        assert_eq!(fs::read_to_string(output).unwrap(), "keep this data\n");
        assert!(
            s.trace().is_empty(),
            "output collision must fail before ADB starts"
        );
    }

    #[test]
    fn foreground_entry_and_duration_limits_reap_adb() {
        for limit in [["--max-entries", "3"], ["--duration-secs", "1"]] {
            let s = Sandbox::new("continuous");
            let output = s.root.join("capture.jsonl");
            let started = Instant::now();
            s.run(&[
                "capture",
                "--serial",
                "TEST-USB",
                "--adb",
                s.adb_path(),
                "--output",
                output.to_str().unwrap(),
                limit[0],
                limit[1],
            ])
            .success();
            assert!(started.elapsed() < Duration::from_secs(4));
            let rows = entries(&fs::read_to_string(output).unwrap());
            if limit[0] == "--max-entries" {
                assert_eq!(rows.len(), 3);
            } else {
                assert!(!rows.is_empty());
            }
            s.assert_reaped();
        }
    }

    #[test]
    fn failed_logcat_and_missing_executable_are_errors() {
        let s = Sandbox::new("stderr_exit");
        let out = s.run(&["capture", "--serial", "TEST-USB", "--adb", s.adb_path()]);
        assert_eq!(out.status.code(), Some(4));
        let document = ciallogcat::document::load(
            out.stdout.as_bytes(),
            4 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(document.entries.is_empty());
        assert_eq!(document.end.capture.state, "failed");
        assert_eq!(document.end.capture.completeness, "partial");
        let error: Value = serde_json::from_str(out.stderr.lines().last().unwrap()).unwrap();
        assert_eq!(error["error"]["code"], "adb_error");
        assert!(out.stderr.contains("fake adb: logcat failed"));
        assert!(
            out.stderr.len() < 32 * 1024,
            "diagnostics must remain bounded"
        );
        s.assert_reaped();
        s.run(&[
            "devices",
            "--adb",
            s.root.join("missing-adb").to_str().unwrap(),
        ])
        .error(4);
    }

    #[test]
    fn capture_byte_limit_keeps_complete_records_inside_budget() {
        let s = Sandbox::new("continuous");
        let output = s.root.join("bounded.jsonl");
        let out = s.run(&[
            "capture",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--output",
            output.to_str().unwrap(),
            "--max-bytes",
            "8192",
        ]);
        out.success();
        let text = fs::read_to_string(output).unwrap();
        assert!(!entries(&text).is_empty());
        assert!(text.ends_with('\n'));
        assert!(text.len() <= 8192, "byte limit exceeded: {}", text.len());
        assert_eq!(out.summary()["bytes"], text.len());
        assert_eq!(out.summary()["entries"], entries(&text).len());
        assert_eq!(out.summary()["omitted_by_limit"], true);
        s.assert_reaped();
    }

    #[test]
    fn closed_stdout_with_live_adb_is_failure_and_reaped() {
        let s = Sandbox::new("eof_open");
        let started = Instant::now();
        let output = s.run(&[
            "capture",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--output",
            s.root.join("eof.jsonl").to_str().unwrap(),
        ]);
        output.error(4);
        assert_eq!(output.summary()["stream_interruptions"], 4);
        assert_eq!(output.summary()["capture_completeness"], "partial");
        assert!(started.elapsed() < Duration::from_secs(12));
        s.assert_reaped();
    }

    #[test]
    fn hung_device_query_times_out_and_reaps_adb() {
        let s = Sandbox::new("query_hang");
        let started = Instant::now();
        let out = s.run(&["devices", "--adb", s.adb_path()]);
        out.error(4);
        assert!(out.stderr.contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(7),
            "fixture watchdog handled timeout instead of production code"
        );
        s.assert_reaped();
    }

    #[test]
    fn hostile_serial_cannot_invoke_a_shell() {
        let s = Sandbox::new("normal");
        let sentinel = s.root.join("shell-was-invoked");
        let serial = format!("TEST-USB;touch {};#", sentinel.display());
        let out = s.run(&[
            "capture",
            "--serial",
            &serial,
            "--adb",
            s.adb_path(),
            "--duration-secs",
            "1",
        ]);
        assert!(!out.status.success());
        assert!(!sentinel.exists());
        for invocation in s.trace() {
            let args = invocation["args"].as_array().unwrap();
            if args.first().is_some_and(|arg| arg == "-s") {
                assert_eq!(args[1], serial, "serial was split or interpreted");
            }
        }
        s.assert_reaped();
    }

    #[test]
    fn foreground_sigint_cleans_up_its_adb_child() {
        let s = Sandbox::new("continuous");
        let output = s.root.join("interrupted.jsonl");
        let diagnostics = s.root.join("interrupted.stderr");
        let mut process = ManagedChild(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args([
                    "capture",
                    "--serial",
                    "TEST-USB",
                    "--adb",
                    s.adb_path(),
                    "--output",
                    output.to_str().unwrap(),
                    "--duration-secs",
                    "30",
                ])
                .env("CIALLOGCAT_FAKE_SCENARIO", "continuous")
                .env("CIALLOGCAT_FAKE_TRACE", &s.trace)
                .env("CIALLOGCAT_CONFIG_DIR", s.root.join("config"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(&diagnostics).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&output).map_or(true, |text| {
            !text
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|row| row["type"] == "entry")
        }) {
            assert!(Instant::now() < deadline, "capture never became ready");
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "capture exited before SIGINT"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Signal only the CLI, as an API client cancelling one capture would.
        // Sending a process-group signal would conceal an orphaned ADB child.
        assert!(
            Command::new("kill")
                .args(["-INT", &process.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let exit = loop {
            if let Some(exit) = process.0.try_wait().unwrap() {
                break exit;
            }
            assert!(Instant::now() < deadline, "capture did not handle SIGINT");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            matches!(exit.code(), Some(0 | 130)),
            "SIGINT did not exit gracefully: {exit}; {}",
            fs::read_to_string(diagnostics).unwrap()
        );
        assert!(!entries(&fs::read_to_string(output).unwrap()).is_empty());
        s.assert_reaped();
    }

    #[test]
    fn closed_consumer_pipe_reports_output_error_and_reaps_adb() {
        let s = Sandbox::new("continuous");
        let diagnostics = s.root.join("closed-pipe.stderr");
        let mut process = ManagedChild(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args([
                    "capture",
                    "--serial",
                    "TEST-USB",
                    "--adb",
                    s.adb_path(),
                    "--duration-secs",
                    "30",
                ])
                .env("CIALLOGCAT_FAKE_SCENARIO", "continuous")
                .env("CIALLOGCAT_FAKE_TRACE", &s.trace)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(fs::File::create(&diagnostics).unwrap())
                .spawn()
                .unwrap(),
        );
        drop(process.0.stdout.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        let exit = loop {
            if let Some(exit) = process.0.try_wait().unwrap() {
                break exit;
            }
            assert!(Instant::now() < deadline, "closed pipe left capture alive");
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = ResultOutput {
            status: exit,
            stdout: String::new(),
            stderr: fs::read_to_string(diagnostics).unwrap(),
        };
        out.error(5);
        assert_eq!(out.summary()["state"], "failed");
        s.assert_reaped();
    }

    #[test]
    fn failed_output_with_idle_adb_exits_before_capture_deadline() {
        let s = Sandbox::new("once_then_idle");
        let diagnostics = s.root.join("full-output.stderr");
        let mut process = ManagedChild(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args([
                    "capture",
                    "--serial",
                    "TEST-USB",
                    "--adb",
                    s.adb_path(),
                    "--duration-secs",
                    "30",
                ])
                .env("CIALLOGCAT_FAKE_SCENARIO", "once_then_idle")
                .env("CIALLOGCAT_FAKE_TRACE", &s.trace)
                .env("CIALLOGCAT_CONFIG_DIR", s.root.join("config"))
                .stdin(Stdio::null())
                // Unlike closing a consumer after spawning, /dev/full fails
                // the very first write without a race over that first record.
                .stdout(
                    fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/full")
                        .unwrap(),
                )
                .stderr(fs::File::create(&diagnostics).unwrap())
                .spawn()
                .unwrap(),
        );
        // Shorter than both the capture's 30 seconds and the fixture's
        // independent 8-second watchdog. No later log can wake capture up.
        let deadline = Instant::now() + Duration::from_secs(5);
        let exit = loop {
            if let Some(exit) = process.0.try_wait().unwrap() {
                break exit;
            }
            assert!(
                Instant::now() < deadline,
                "idle ADB concealed output failure"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = ResultOutput {
            status: exit,
            stdout: String::new(),
            stderr: fs::read_to_string(diagnostics).unwrap(),
        };
        let failure = out.error(5);
        assert_eq!(failure["error"]["code"], "output_error");
        let expected = std::io::Error::from_raw_os_error(28).to_string();
        assert_eq!(failure["error"]["message"], expected);
        let summary = out.summary();
        assert_eq!(summary["error"], expected);
        assert_eq!(summary["state"], "failed");
        assert_eq!(summary["entries"], 0);
        assert_eq!(summary["bytes"], 0);
        assert_eq!(summary["diagnostics_complete"], true);
        s.assert_reaped();
    }

    fn start(s: &Sandbox, bounds: &[&str]) -> Value {
        let session = s.session();
        let mut args = vec![
            "start",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--session-dir",
            session.to_str().unwrap(),
        ];
        args.extend_from_slice(bounds);
        s.run(&args).success().json()
    }
    fn status(s: &Sandbox) -> Value {
        s.run(&["status", "--session-dir", s.session().to_str().unwrap()])
            .success()
            .json()
    }
    fn wait_status(s: &Sandbox, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let state = status(s);
            if predicate(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "session state did not converge: {state}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn background_start_query_stop_is_idempotent_and_reaps_adb() {
        let s = Sandbox::new("continuous");
        let initial = start(&s, &["--duration-secs", "30"]);
        assert_eq!(initial["type"], "status");
        wait_status(&s, |state| state["entries"].as_u64().unwrap_or(0) >= 4);
        let query = s
            .run(&[
                "query",
                "--session-dir",
                s.session().to_str().unwrap(),
                "--limit",
                "2",
            ])
            .success()
            .json();
        assert_eq!(query["type"], "query");
        let first = query["records"].as_array().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(query["has_more"], true);
        let cursor = query["next_cursor"].as_str().unwrap();
        let next = s
            .run(&[
                "query",
                "--session-dir",
                s.session().to_str().unwrap(),
                "--cursor",
                cursor,
                "--limit",
                "2",
            ])
            .success()
            .json();
        assert!(next["records"][0]["seq"].as_u64().unwrap() > first[1]["seq"].as_u64().unwrap());
        assert_eq!(next["snapshot"], query["snapshot"]);
        for _ in 0..2 {
            let stopped = s
                .run(&["stop", "--session-dir", s.session().to_str().unwrap()])
                .success()
                .json();
            assert_eq!(stopped["state"], "completed");
            assert_eq!(stopped["reason"], "stopped");
        }
        assert_eq!(status(&s)["state"], "completed");
        s.assert_reaped();
    }

    #[test]
    fn bounded_background_session_completes_and_rejects_reuse() {
        let s = Sandbox::new("continuous");
        start(&s, &["--max-entries", "3"]);
        let done = wait_status(&s, |state| state["state"] == "completed");
        assert_eq!(done["entries"], 3);
        assert_eq!(done["reason"], "max_entries");
        let query = s
            .run(&["query", "--session-dir", s.session().to_str().unwrap()])
            .success()
            .json();
        assert_eq!(query["records"].as_array().unwrap().len(), 3);
        assert_eq!(query["complete"], true);
        let before = fs::read(s.session().join("logs.jsonl")).unwrap();
        let repeat = s.run(&[
            "start",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--session-dir",
            s.session().to_str().unwrap(),
        ]);
        assert!(!repeat.status.success());
        assert_eq!(fs::read(s.session().join("logs.jsonl")).unwrap(), before);
        s.assert_reaped();
    }

    #[test]
    fn background_failure_retains_terminal_diagnostics() {
        let s = Sandbox::new("stderr_exit");
        let out = s.run(&[
            "start",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--session-dir",
            s.session().to_str().unwrap(),
        ]);
        // The ADB process can exit between the readiness event and the start
        // command's next status read. Both outcomes must retain terminal state.
        if !out.status.success() {
            out.error(4);
        }
        let failed = wait_status(&s, |state| state["state"] == "failed");
        assert_eq!(failed["entries"], 0);
        assert_eq!(failed["diagnostics_complete"], true);
        assert_eq!(failed["last_diagnostic"], "fake adb: logcat failed");
        let interruptions = failed["stream_interruptions"].as_u64().unwrap();
        assert_eq!(interruptions, 4, "initial attempt plus three retries");
        assert_eq!(failed["diagnostic_lines"], interruptions * 3);
        assert_eq!(failed["capture_completeness"], "partial");
        let query = s
            .run(&["query", "--session-dir", s.session().to_str().unwrap()])
            .success()
            .json();
        assert!(query["records"].as_array().unwrap().is_empty());
        s.assert_reaped();
    }

    #[test]
    fn unknown_session_is_error_and_never_signals_unrelated_processes() {
        let s = Sandbox::new("normal");
        for verb in ["status", "query", "stop"] {
            s.run(&[verb, "--session-dir", s.session().to_str().unwrap()])
                .error(3);
        }
        assert!(s.trace().is_empty());
    }

    #[test]
    fn stale_metadata_never_signals_a_persisted_pid() {
        let s = Sandbox::new("normal");
        let mut unrelated = ManagedChild(Command::new("sleep").arg("30").spawn().unwrap());
        fs::create_dir(s.session()).unwrap();
        let forged = serde_json::json!({
            "schema_version":1,"type":"status","session":s.session(),
            "worker_pid":unrelated.0.id(),"state":"running","readiness":"adb_process_started",
            "entries":0,"bytes":0,"reason":null,"error":null,"updated_unix_ms":0,"stale":false,
            "omitted_by_limit":false,"diagnostic_lines":0,"last_diagnostic":null,"diagnostics_complete":false,
            "diagnostic_dropped_count":0,"discarded_entries":0
        });
        fs::write(
            s.session().join("status.json"),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        let current = status(&s);
        assert_eq!(current["state"], "unknown");
        assert_eq!(current["stale"], true);
        s.run(&[
            "stop",
            "--session-dir",
            s.session().to_str().unwrap(),
            "--wait-secs",
            "1",
        ])
        .error(6);
        assert!(s.session().join("stop").is_file());
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "stop signalled an unrelated persisted PID"
        );
        assert!(s.trace().is_empty());
    }

    #[test]
    fn query_exhausted_cursor_unpublished_tail_and_corrupt_prefix_are_distinct() {
        let s = Sandbox::new("continuous");
        start(&s, &["--max-entries", "3"]);
        let completed = wait_status(&s, |state| state["state"] == "completed");
        s.assert_reaped();
        let logs = s.session().join("logs.jsonl");
        let original = fs::read(&logs).unwrap();
        let footer_start = original[..original.len() - 1]
            .iter()
            .rposition(|&b| b == b'\n')
            .unwrap()
            + 1;
        let mut published = completed.clone();
        published["bytes"] = footer_start.into();
        published["visible_end"]["offset"] = footer_start.into();
        fs::write(
            s.session().join("status.json"),
            serde_json::to_vec(&published).unwrap(),
        )
        .unwrap();
        let first = s
            .run(&["query", "--session-dir", s.session().to_str().unwrap()])
            .success()
            .json();
        assert_eq!(first["records"].as_array().unwrap().len(), 3);
        assert_eq!(first["complete"], true);
        let empty = s
            .run(&[
                "query",
                "--session-dir",
                s.session().to_str().unwrap(),
                "--cursor",
                first["next_cursor"].as_str().unwrap(),
            ])
            .success()
            .json();
        assert!(empty["records"].as_array().unwrap().is_empty());
        assert_eq!(empty["scan"]["last_seq"], 3);
        assert_eq!(empty["scan"]["records"], 0);
        assert_eq!(empty["snapshot"], first["snapshot"]);
        assert_eq!(empty["complete"], true);
        let mut partial = original[..footer_start].to_vec();
        partial.extend_from_slice(b"{\"seq\":4");
        for terminated in [false, true] {
            if terminated {
                partial.push(b'\n');
            }
            fs::write(&logs, &partial).unwrap();
            // Even a complete corrupt line beyond visible_end.offset is not published.
            let query = s
                .run(&["query", "--session-dir", s.session().to_str().unwrap()])
                .success()
                .json();
            assert_eq!(query["records"].as_array().unwrap().len(), 3);
            assert_eq!(query["snapshot"], first["snapshot"]);
            assert_eq!(query["complete"], true);

            let mut corrupt_status = published.clone();
            corrupt_status["entries"] = 4.into();
            corrupt_status["bytes"] = partial.len().into();
            corrupt_status["visible_end"] = serde_json::json!({"seq":4,"offset":partial.len()});
            fs::write(
                s.session().join("status.json"),
                serde_json::to_vec(&corrupt_status).unwrap(),
            )
            .unwrap();
            let error = s
                .run(&["query", "--session-dir", s.session().to_str().unwrap()])
                .error(3);
            assert_eq!(error["error"]["code"], "invalid_source");
            fs::write(
                s.session().join("status.json"),
                serde_json::to_vec(&published).unwrap(),
            )
            .unwrap();
        }
        fs::write(logs, original).unwrap();
        fs::write(
            s.session().join("status.json"),
            serde_json::to_vec(&completed).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn query_literal_filter_advances_cursor_even_with_zero_matches() {
        let s = Sandbox::new("continuous");
        start(&s, &["--max-entries", "3"]);
        wait_status(&s, |state| state["state"] == "completed");
        for (needle, expected) in [
            ("continuous-000001", 1),
            ("FakeAdb", 0),
            ("continuous", 3),
            ("fakeadb", 0),
            (".*", 0),
        ] {
            let query = s
                .run(&[
                    "query",
                    "--session-dir",
                    s.session().to_str().unwrap(),
                    "--query",
                    needle,
                ])
                .success()
                .json();
            assert_eq!(
                query["records"].as_array().unwrap().len(),
                expected,
                "needle={needle}"
            );
            assert_eq!(
                query["scan"]["last_seq"], 3,
                "filter must advance over scanned nonmatches"
            );
            assert_eq!(query["has_more"], false);
            assert_eq!(query["complete"], true);
            assert_eq!(query["scan"]["records"], 3);
            assert_eq!(query["scan"]["stop_reason"], "upper_bound");
            let resumed = s
                .run(&[
                    "query",
                    "--session-dir",
                    s.session().to_str().unwrap(),
                    "--query",
                    needle,
                    "--cursor",
                    query["next_cursor"].as_str().unwrap(),
                ])
                .success()
                .json();
            assert!(resumed["records"].as_array().unwrap().is_empty());
            assert_eq!(resumed["scan"]["records"], 0);
            assert_eq!(resumed["scan"]["last_seq"], 3);
        }
        s.assert_reaped();
    }
}

#[cfg(target_os = "linux")]
mod device_diagnostics {
    use super::*;

    fn observation<'a>(report: &'a Value, name: &str) -> &'a Value {
        report["observations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["probe"] == name)
            .unwrap()
    }

    #[test]
    fn diagnose_reads_only_fixed_queries_and_preserves_actual_observations() {
        let sandbox = Sandbox::new("normal");
        let output = sandbox.run(&[
            "diagnose",
            "--serial",
            "TEST-USB",
            "--buffers",
            "events,radio",
        ]);
        let report = output.success().json();
        assert_eq!(report["type"], "device_diagnostics");
        assert_eq!(
            report["requested_buffers"],
            serde_json::json!(["events", "radio"])
        );
        assert_eq!(report["shell_v2"], "advertised");
        assert_eq!(report["complete"], true);
        assert_eq!(report["cancelled"], false);
        assert!(output.stderr.is_empty());
        assert_eq!(
            observation(&report, "android_sdk")["stdout"]["text"],
            "35\n"
        );
        assert_eq!(
            observation(&report, "prune_rules")["stdout"]["text"],
            "~! ~1000/!\n"
        );
        let help = observation(&report, "logcat_help")["stdout"]["text"]
            .as_str()
            .unwrap();
        assert!(help.starts_with("Usage: logcat"));
        assert!(!help.contains("nsec"));
        assert!(!help.contains("proto"));
        assert!(
            observation(&report, "buffer_sizes")["stdout"]["text"]
                .as_str()
                .unwrap()
                .contains("ring buffer is 256 KiB")
        );
        assert_eq!(report["max_output_bytes_per_stream"], 16384);
        assert!(output.stdout.len() <= report["max_json_bytes"].as_u64().unwrap() as usize);
        let expected = [
            vec!["features"],
            vec!["shell", "-T", "-n", "exec logcat --help"],
            vec!["shell", "-T", "-n", "exec logcat -b events -b radio -g"],
            vec!["shell", "-T", "-n", "exec logcat -b events -b radio -S"],
            vec!["shell", "-T", "-n", "exec logcat -b events -b radio -p"],
            vec!["shell", "-T", "-n", "getprop ro.build.version.sdk"],
            vec!["shell", "-T", "-n", "getprop ro.build.version.release"],
            vec!["shell", "-T", "-n", "getprop ro.build.fingerprint"],
        ];
        let trace = sandbox.trace();
        assert_eq!(trace.len(), expected.len());
        for ((row, suffix), item) in trace
            .iter()
            .zip(expected)
            .zip(report["observations"].as_array().unwrap())
        {
            let args: Vec<_> = ["-s", "TEST-USB"].into_iter().chain(suffix).collect();
            assert_eq!(row["args"], serde_json::json!(args));
            assert_eq!(item["arguments"], row["args"]);
            assert_eq!(item["exit"]["code"], 0);
            assert_eq!(item["status"], "success");
            assert_eq!(item["stdout"]["eof"], true);
            assert_eq!(item["stderr"]["text"], "");
            assert!(
                item["started_unix_ms"].as_u64().unwrap()
                    <= item["finished_unix_ms"].as_u64().unwrap()
            );
        }
        sandbox.assert_reaped();
    }

    #[test]
    fn diagnose_failures_are_independent_and_keep_both_streams_and_exit_code() {
        let sandbox = Sandbox::new("diagnostic_partial");
        let output = sandbox.run(&["diagnose", "--serial", "TEST-USB"]);
        let report = output.success().json();
        assert_eq!(report["complete"], false);
        let statistics = observation(&report, "statistics");
        assert_eq!(statistics["status"], "permission_denied");
        assert_eq!(statistics["exit"]["code"], 13);
        assert_eq!(statistics["stdout"]["text"], "partial statistics header\n");
        assert_eq!(
            statistics["stderr"]["text"],
            "logcat: failed to read statistics: Permission denied\n"
        );
        assert_eq!(observation(&report, "prune_rules")["status"], "unsupported");
        assert_eq!(observation(&report, "android_release")["status"], "unknown");
        for name in [
            "adb_features",
            "logcat_help",
            "buffer_sizes",
            "android_sdk",
            "build_fingerprint",
        ] {
            assert_eq!(observation(&report, name)["status"], "success");
        }
        assert_eq!(sandbox.trace().len(), 8);
        sandbox.assert_reaped();
    }

    #[test]
    fn diagnose_help_failures_and_stderr_help_do_not_erase_other_data() {
        for (scenario, status) in [
            ("diagnostic_permission", "permission_denied"),
            ("diagnostic_unsupported", "unsupported"),
            ("diagnostic_help_stderr", "success"),
        ] {
            let sandbox = Sandbox::new(scenario);
            let output = sandbox.run(&["diagnose", "--serial", "TEST-USB"]);
            let report = output.success().json();
            let help = observation(&report, "logcat_help");
            assert_eq!(help["status"], status);
            assert_eq!(help["stdout"]["text"], "");
            assert!(!help["stderr"]["text"].as_str().unwrap().is_empty());
            assert_eq!(observation(&report, "android_sdk")["status"], "success");
            sandbox.assert_reaped();
        }
    }

    #[test]
    fn diagnose_classification_ignores_error_phrases_in_missing_device_serials() {
        for serial in [
            "permission denied",
            "unknown option",
            "serial': permission denied",
            "serial\" unknown option \"",
        ] {
            let sandbox = Sandbox::new("diagnostic_missing_device");
            let report = sandbox
                .run(&["diagnose", "--serial", serial])
                .success()
                .json();
            assert_eq!(report["serial"], serial);
            assert_eq!(report["complete"], false);
            assert_eq!(report["shell_v2"], "unknown");
            assert_eq!(report["observations"].as_array().unwrap().len(), 8);
            for item in report["observations"].as_array().unwrap() {
                let prefix = if item["probe"] == "adb_features" {
                    "adb: error: failed to get feature set: "
                } else {
                    "error: "
                };
                assert_eq!(item["status"], "unknown", "serial={serial:?}, {item}");
                assert_eq!(item["attempted"], true);
                assert_eq!(item["arguments"][1], serial);
                assert_eq!(item["exit"]["code"], 1);
                assert_eq!(item["stdout"]["text"], "");
                assert_eq!(
                    item["stderr"]["text"],
                    format!("{prefix}device '{serial}' not found\n")
                );
                assert_eq!(item["stderr"]["eof"], true);
            }
            assert_eq!(sandbox.trace().len(), 8);
            assert!(sandbox.trace().iter().all(|row| row["args"][1] == serial));
            sandbox.assert_reaped();
        }
    }

    #[test]
    fn diagnose_classification_requires_error_statements_in_either_stream() {
        let sandbox = Sandbox::new("diagnostic_error_statements");
        let report = sandbox
            .run(&["diagnose", "--serial", "TEST-USB"])
            .success()
            .json();
        assert_eq!(report["complete"], false);
        for (probe, status, stdout, stderr) in [
            (
                "logcat_help",
                "unknown",
                "Usage example: 'permission denied' / 'unknown option'\n",
                "logcat: unexpected failure\n",
            ),
            (
                "buffer_sizes",
                "unsupported",
                "",
                "logcat: unrecognized option '--permission denied'\n",
            ),
            (
                "statistics",
                "permission_denied",
                "logcat: Permission denied\n",
                "",
            ),
            (
                "prune_rules",
                "unsupported",
                "logcat: unknown option '-p'\n",
                "",
            ),
            (
                "android_sdk",
                "unknown",
                "",
                "诊断失败：permission denied\n",
            ),
            (
                "android_release",
                "unknown",
                "",
                "logcat: unknown optional format\n",
            ),
            (
                "build_fingerprint",
                "unknown",
                "",
                "logcat: failed to inspect 'permission denied': unexpected failure\n",
            ),
        ] {
            let item = observation(&report, probe);
            assert_eq!(item["status"], status, "{probe}: {item}");
            assert_eq!(item["exit"]["code"], 1);
            assert_eq!(item["stdout"]["text"], stdout);
            assert_eq!(item["stderr"]["text"], stderr);
        }
        assert_eq!(observation(&report, "adb_features")["status"], "success");
        assert_eq!(sandbox.trace().len(), 8);
        sandbox.assert_reaped();
    }

    #[test]
    fn diagnose_classifies_complete_lines_without_promoting_truncated_prefixes() {
        for cap in [13, 17, 18, 20, 65536] {
            let sandbox = Sandbox::new("diagnostic_truncated_statements");
            let report = sandbox
                .run(&[
                    "diagnose",
                    "--serial",
                    "TEST-USB",
                    "--max-output-bytes",
                    &cap.to_string(),
                ])
                .success()
                .json();
            for probe in ["logcat_help", "buffer_sizes", "build_fingerprint"] {
                assert_eq!(
                    observation(&report, probe)["status"],
                    "unknown",
                    "cap={cap}, {probe}"
                );
            }
            for (probe, required, status) in [
                ("statistics", 18, "permission_denied"),
                ("prune_rules", 20, "unsupported"),
                ("android_release", 14, "unsupported"),
                // A complete EOF without a final LF is still a whole reason.
                ("android_sdk", 17, "permission_denied"),
            ] {
                let expected = if cap >= required { status } else { "unknown" };
                assert_eq!(
                    observation(&report, probe)["status"],
                    expected,
                    "cap={cap}, {probe}"
                );
            }
            let help = observation(&report, "logcat_help");
            assert_eq!(help["stderr"]["truncated"], cap < 90);
            assert_eq!(help["stderr"]["observed_bytes"], 90);
            assert_eq!(help["stderr"]["retained_bytes"], cap.min(90));
            if cap == 17 {
                assert_eq!(help["stderr"]["text"], "Permission denied");
            }
            sandbox.assert_reaped();
        }
    }

    #[test]
    fn diagnose_caps_streams_independently_and_bounds_escaped_json() {
        let sandbox = Sandbox::new("diagnostic_large");
        let output = sandbox.run(&[
            "diagnose",
            "--serial",
            "TEST-USB",
            "--max-output-bytes",
            "128",
        ]);
        let report = output.success().json();
        let help = observation(&report, "logcat_help");
        assert_eq!(help["status"], "success");
        assert_eq!(help["exit"]["code"], 0);
        assert!(
            help["stdout"]["text"]
                .as_str()
                .unwrap()
                .starts_with("OUT\0\"\\\n")
        );
        assert!(
            help["stderr"]["text"]
                .as_str()
                .unwrap()
                .starts_with("ERR\u{fffd}\r\t\n")
        );
        for stream in ["stdout", "stderr"] {
            assert_eq!(help[stream]["retained_bytes"], 128);
            assert_eq!(help[stream]["observed_bytes"], 7 * 32768);
            assert_eq!(help[stream]["truncated"], true);
            assert_eq!(help[stream]["eof"], true);
        }
        assert_eq!(help["stdout"]["lossy_utf8"], false);
        assert_eq!(help["stderr"]["lossy_utf8"], true);
        assert_eq!(report["complete"], false);
        assert!(output.stdout.len() <= report["max_json_bytes"].as_u64().unwrap() as usize);
        assert_eq!(
            observation(&report, "build_fingerprint")["status"],
            "success"
        );
        sandbox.assert_reaped();
    }

    #[test]
    fn diagnose_does_not_infer_capability_from_missing_or_truncated_features() {
        for (scenario, cap, capability) in [
            ("proto_no_shell_v2", "16384", "not_advertised"),
            ("proto_feature_error", "16384", "unknown"),
            ("normal", "12", "unknown"),
        ] {
            let sandbox = Sandbox::new(scenario);
            let output = sandbox.run(&[
                "diagnose",
                "--serial",
                "TEST-USB",
                "--max-output-bytes",
                cap,
            ]);
            let report = output.success().json();
            assert_eq!(report["shell_v2"], capability);
            assert_eq!(observation(&report, "android_sdk")["status"], "success");
            sandbox.assert_reaped();
        }
    }

    #[test]
    fn diagnose_per_probe_timeout_preserves_prior_data_and_continues() {
        for scenario in ["diagnostic_silent", "diagnostic_stderr_forever"] {
            let sandbox = Sandbox::new(scenario);
            let start = Instant::now();
            let output = sandbox.run(&[
                "diagnose",
                "--serial",
                "TEST-USB",
                "--probe-timeout-ms",
                "250",
                "--timeout-ms",
                "3000",
                "--max-output-bytes",
                "64",
            ]);
            let report = output.success().json();
            assert!(start.elapsed() < Duration::from_secs(3));
            assert_eq!(report["deadline_exceeded"], false);
            let help = observation(&report, "logcat_help");
            assert_eq!(help["status"], "timed_out");
            assert_eq!(help["attempted"], true);
            assert_eq!(help["exit"]["success"], false);
            if scenario == "diagnostic_stderr_forever" {
                assert_eq!(help["stderr"]["truncated"], true);
                assert_eq!(help["stderr"]["retained_bytes"], 64);
            }
            assert_eq!(observation(&report, "adb_features")["status"], "success");
            assert_eq!(
                observation(&report, "build_fingerprint")["status"],
                "success"
            );
            sandbox.assert_reaped();
        }
    }

    #[test]
    fn diagnose_total_deadline_is_not_reset_by_each_query() {
        let sandbox = Sandbox::new("diagnostic_slow");
        let start = Instant::now();
        let output = sandbox.run(&[
            "diagnose",
            "--serial",
            "TEST-USB",
            "--timeout-ms",
            "450",
            "--probe-timeout-ms",
            "2000",
        ]);
        let report = output.success().json();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(report["deadline_exceeded"], true);
        assert_eq!(report["complete"], false);
        assert_eq!(observation(&report, "adb_features")["status"], "success");
        assert_eq!(
            observation(&report, "build_fingerprint")["status"],
            "timed_out"
        );
        assert_eq!(
            observation(&report, "build_fingerprint")["attempted"],
            false
        );
        assert!(sandbox.trace().len() < 8);
        sandbox.assert_reaped();
    }

    #[test]
    fn diagnose_ctrl_c_cancels_silent_and_flooding_children_and_reaps_before_return() {
        for scenario in ["diagnostic_silent", "diagnostic_stderr_forever"] {
            let sandbox = Sandbox::new(scenario);
            let stdout = sandbox.root.join("cancel.stdout");
            let stderr = sandbox.root.join("cancel.stderr");
            let mut child = ManagedChild(
                Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                    .args([
                        "diagnose",
                        "--serial",
                        "TEST-USB",
                        "--probe-timeout-ms",
                        "6000",
                        "--max-output-bytes",
                        "128",
                    ])
                    .env("CIALLOGCAT_ADB", &sandbox.adb)
                    .env("CIALLOGCAT_FAKE_SCENARIO", scenario)
                    .env("CIALLOGCAT_FAKE_TRACE", &sandbox.trace)
                    .stdin(Stdio::null())
                    .stdout(fs::File::create(&stdout).unwrap())
                    .stderr(fs::File::create(&stderr).unwrap())
                    .spawn()
                    .unwrap(),
            );
            let ready_by = Instant::now() + Duration::from_secs(3);
            while !sandbox.trace().iter().any(|row| {
                matches!(
                    row["event"].as_str(),
                    Some("diagnostic_idle" | "diagnostic_flood")
                )
            }) {
                assert!(
                    Instant::now() < ready_by,
                    "fixture never reached the blocking probe"
                );
                assert!(child.0.try_wait().unwrap().is_none());
                std::thread::sleep(Duration::from_millis(10));
            }
            // Only signal the CLI; signalling the group could hide an ADB leak.
            assert!(
                Command::new("kill")
                    .args(["-INT", &child.0.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert_eq!(status.code(), Some(130));
                    break;
                }
                assert!(Instant::now() < deadline, "diagnose ignored Ctrl-C");
                std::thread::sleep(Duration::from_millis(10));
            }
            let report: Value = serde_json::from_slice(&fs::read(stdout).unwrap()).unwrap();
            assert_eq!(report["cancelled"], true);
            assert_eq!(observation(&report, "logcat_help")["status"], "cancelled");
            assert_eq!(observation(&report, "adb_features")["status"], "success");
            assert_eq!(
                observation(&report, "build_fingerprint")["attempted"],
                false
            );
            assert_eq!(
                sandbox
                    .trace()
                    .iter()
                    .filter(|r| r["event"] == "invocation")
                    .count(),
                2
            );
            sandbox.assert_reaped();
        }
    }

    #[test]
    fn diagnose_validates_flags_and_has_focused_help_without_adb() {
        let sandbox = Sandbox::new("normal");
        let help = sandbox.run(&["diagnose", "--help"]);
        help.success();
        for phrase in [
            "--probe-timeout-ms",
            "--max-output-bytes",
            "shell_v2",
            "SDK level",
        ] {
            assert!(help.stdout.contains(phrase));
        }
        assert!(!help.stdout.contains("query (--file"));
        for (flag, value) in [
            ("--buffers", "main,all"),
            ("--buffers", "main;reboot"),
            ("--timeout-ms", "0"),
            ("--probe-timeout-ms", "60001"),
            ("--max-output-bytes", "65537"),
            ("--command", "logcat -c"),
        ] {
            sandbox
                .run(&["diagnose", "--serial", "TEST-USB", flag, value])
                .error(2);
        }
        sandbox
            .run(&["diagnose", "--serial", "bad\nserial"])
            .error(2);
        assert!(sandbox.trace().is_empty());
    }

    #[test]
    fn diagnose_missing_executable_is_per_probe_unknown_not_lost_report() {
        let sandbox = Sandbox::new("normal");
        let missing = sandbox.root.join("nonexistent-adb");
        let output = sandbox.run(&[
            "diagnose",
            "--serial",
            "TEST-USB",
            "--adb",
            missing.to_str().unwrap(),
        ]);
        let report = output.success().json();
        assert_eq!(report["shell_v2"], "unknown");
        assert_eq!(report["complete"], false);
        for item in report["observations"].as_array().unwrap() {
            assert_eq!(item["status"], "unknown");
            assert!(item["exit"].is_null());
            assert!(item["error"].is_string());
        }
        assert!(sandbox.trace().is_empty());
    }
}

#[test]
fn worker_rejects_native_budgets_before_output() {
    for transport in ["text", "proto"] {
        let budget = 8191;
        let s = Sandbox::new("normal");
        let dir = s.session();
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({
                "adb":s.adb.to_str().unwrap(), "serial":"TEST-USB", "duration_secs":1,
                "max_bytes":budget, "max_entries":1,
                "transport":transport,
                "scope":ciallogcat::capture_scope::CaptureScope::default(),
                "buffers":ciallogcat::model::LogBuffer::defaults()
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            s.run(&["__worker", "--session-dir", dir.to_str().unwrap()])
                .error(3)["error"]["code"],
            "invalid_session"
        );
        assert!(!dir.join("logs.jsonl").exists());
        assert!(s.trace().is_empty());
    }
}

#[test]
fn worker_rejects_incomplete_config_before_output_or_adb() {
    for field in ["transport", "scope", "buffers"] {
        let s = Sandbox::new("normal");
        let dir = s.session();
        fs::create_dir(&dir).unwrap();
        let mut config = serde_json::json!({
            "adb":s.adb.to_str().unwrap(), "serial":"TEST-USB", "duration_secs":1,
            "max_bytes":8192, "max_entries":1,
            "transport":"text", "scope":ciallogcat::capture_scope::CaptureScope::default(),
            "buffers":ciallogcat::model::LogBuffer::defaults()
        });
        config.as_object_mut().unwrap().remove(field);
        fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let error = s
            .run(&["__worker", "--session-dir", dir.to_str().unwrap()])
            .error(3);
        assert_eq!(error["error"]["code"], "invalid_session");
        assert!(error["error"]["message"].as_str().unwrap().contains(field));
        assert!(!dir.join("logs.jsonl").exists());
        assert!(s.trace().is_empty());
    }
}

#[test]
fn worker_rejects_obsolete_format_field_in_saved_config() {
    let s = Sandbox::new("normal");
    let dir = s.session();
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("config.json"),
        serde_json::json!({
            "adb":s.adb.to_str().unwrap(), "serial":"TEST-USB", "duration_secs":1,
            "max_bytes":8192, "max_entries":1, "record_schema_version":3,
            "transport":"text", "scope":ciallogcat::capture_scope::CaptureScope::default(),
            "buffers":ciallogcat::model::LogBuffer::defaults()
        })
        .to_string(),
    )
    .unwrap();
    let error = s
        .run(&["__worker", "--session-dir", dir.to_str().unwrap()])
        .error(3);
    assert_eq!(error["error"]["code"], "invalid_session");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("record_schema_version")
    );
    assert!(!dir.join("logs.jsonl").exists());
    assert!(s.trace().is_empty());
}
