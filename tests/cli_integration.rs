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
    for command in ["devices", "capture", "start", "status", "query", "stop"] {
        assert!(help.stdout.contains(command));
    }
    let version = s.run(&["--version"]);
    version.success();
    assert!(version.stdout.contains(env!("CARGO_PKG_VERSION")));
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
        text.lines()
            .map(|line| {
                let value: Value = serde_json::from_str(line).unwrap();
                assert_eq!(value["schema_version"], 1);
                assert_eq!(value["type"], "entry");
                assert!(value["seq"].is_u64());
                for key in ["time", "level", "tag", "message"] {
                    assert!(value[key].is_string(), "missing {key}: {value}");
                }
                assert!(value["pid"].is_u64());
                assert!(value["tid"].is_u64());
                value
            })
            .collect()
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
        let out = s.run(&[
            "capture",
            "--serial",
            "emulator-5554",
            "--adb",
            s.adb_path(),
            "--max-entries",
            "2",
        ]);
        let rows = entries(&out.success().stdout);
        assert_eq!(rows.len(), 2);
        let summary = out.summary();
        assert_eq!(summary["entries"], rows.len());
        assert_eq!(summary["bytes"], out.stdout.len());
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
        out.error(4);
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
            "1024",
        ]);
        out.success();
        let text = fs::read_to_string(output).unwrap();
        assert!(!entries(&text).is_empty());
        assert!(text.ends_with('\n'));
        assert!(text.len() <= 1024, "byte limit exceeded: {}", text.len());
        assert_eq!(out.summary()["bytes"], text.len());
        assert_eq!(out.summary()["entries"], entries(&text).len());
        assert_eq!(out.summary()["omitted_by_limit"], true);
        s.assert_reaped();
    }

    #[test]
    fn closed_stdout_with_live_adb_is_failure_and_reaped() {
        let s = Sandbox::new("eof_open");
        let started = Instant::now();
        s.run(&[
            "capture",
            "--serial",
            "TEST-USB",
            "--adb",
            s.adb_path(),
            "--output",
            s.root.join("eof.jsonl").to_str().unwrap(),
        ])
        .error(4);
        assert!(started.elapsed() < Duration::from_secs(4));
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
        while fs::metadata(&output).map_or(true, |metadata| metadata.len() == 0) {
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
        let first = query["entries"].as_array().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(query["has_more"], true);
        let cursor = query["next_after_seq"].as_u64().unwrap().to_string();
        let next = s
            .run(&[
                "query",
                "--session-dir",
                s.session().to_str().unwrap(),
                "--after-seq",
                &cursor,
                "--limit",
                "2",
            ])
            .success()
            .json();
        assert!(next["entries"][0]["seq"].as_u64().unwrap() > cursor.parse::<u64>().unwrap());
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
        assert_eq!(query["entries"].as_array().unwrap().len(), 3);
        assert_eq!(query["partial"], false);
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
        assert_eq!(failed["diagnostic_lines"], 3);
        let query = s
            .run(&["query", "--session-dir", s.session().to_str().unwrap()])
            .success()
            .json();
        assert!(query["entries"].as_array().unwrap().is_empty());
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
        s.run(&["stop", "--session-dir", s.session().to_str().unwrap()])
            .error(3);
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "stop signalled an unrelated persisted PID"
        );
        assert!(s.trace().is_empty());
    }

    #[test]
    fn query_empty_cursor_partial_tail_and_corrupt_record_are_distinct() {
        let s = Sandbox::new("continuous");
        start(&s, &["--max-entries", "3"]);
        wait_status(&s, |state| state["state"] == "completed");
        s.assert_reaped();
        let logs = s.session().join("logs.jsonl");
        let original = fs::read(&logs).unwrap();
        let empty = s
            .run(&[
                "query",
                "--session-dir",
                s.session().to_str().unwrap(),
                "--after-seq",
                "999",
            ])
            .success()
            .json();
        assert!(empty["entries"].as_array().unwrap().is_empty());
        assert_eq!(empty["next_after_seq"], 999);
        assert_eq!(empty["partial"], false);
        let mut partial = original.clone();
        partial.extend_from_slice(b"{\"seq\":4");
        fs::write(&logs, &partial).unwrap();
        let query = s
            .run(&["query", "--session-dir", s.session().to_str().unwrap()])
            .success()
            .json();
        assert_eq!(query["entries"].as_array().unwrap().len(), 3);
        assert_eq!(query["partial"], true);
        partial.push(b'\n');
        fs::write(&logs, &partial).unwrap();
        s.run(&["query", "--session-dir", s.session().to_str().unwrap()])
            .error(3);
        fs::write(logs, original).unwrap();
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
                query["entries"].as_array().unwrap().len(),
                expected,
                "needle={needle}"
            );
            assert_eq!(
                query["next_after_seq"], 3,
                "filter must advance over scanned nonmatches"
            );
            assert_eq!(query["has_more"], false);
            assert_eq!(query["scan_limited"], false);
        }
        s.assert_reaped();
    }
}
