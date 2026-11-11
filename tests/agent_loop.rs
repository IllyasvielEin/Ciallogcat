//! Agent-loop contract: marks, waiting tails, crash grouping, PID context,
//! package warnings and compact tokens. Every CLI call has a deadline.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ciallogcat::model::{Level, LogEntry};
use serde_json::{Value, json};

#[path = "support/native_fixture.rs"]
mod native_fixture;

struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Output {
    ok: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

type Row<'a> = (u32, &'a str, Level, &'a str);

impl Sandbox {
    fn new() -> Self {
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .join("target/agent-loop")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(root.join("session")).unwrap();
        Self { root }
    }

    fn dir(&self) -> String {
        self.root.join("session").to_str().unwrap().into()
    }

    /// Publish the given rows as seq 1..=n of a running native session.
    fn publish(&self, rows: &[Row<'_>]) {
        let mut bytes = native_fixture::header_bytes();
        for (index, &(pid, tag, level, message)) in rows.iter().enumerate() {
            let seq = index as u64 + 1;
            let entry = LogEntry::new("11-11 00:00:00.000", pid, pid + 1, tag, level, message);
            let row = native_fixture::entry_row(seq, entry, Some(1_700_000_000_000 + seq));
            bytes.extend(native_fixture::line(&row));
        }
        fs::write(self.root.join("session/logs.jsonl"), &bytes).unwrap();
        let mut status = json!({"schema_version":1,"type":"status","state":"running",
            "updated_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_millis() as u64});
        native_fixture::set_status(&mut status, rows.len() as u64, bytes.len() as u64);
        let temporary = self.root.join("session/status.json.tmp");
        fs::write(&temporary, serde_json::to_vec(&status).unwrap()).unwrap();
        fs::rename(temporary, self.root.join("session/status.json")).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let id = uuid::Uuid::new_v4();
        let out = self.root.join(format!("{id}.out"));
        let err = self.root.join(format!("{id}.err"));
        let mut child = Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
            .args(args)
            .env("CIALLOGCAT_CONFIG_DIR", self.root.join("config"))
            .stdin(Stdio::null())
            .stdout(File::create(&out).unwrap())
            .stderr(File::create(&err).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("CLI timeout: {args:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Output {
            ok: status.success(),
            stdout: fs::read(out).unwrap(),
            stderr: fs::read(err).unwrap(),
        }
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.ok,
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn fail(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(!output.ok, "unexpected success: {args:?}");
        let value: Value = serde_json::from_slice(&output.stderr).unwrap();
        value["error"]["code"].as_str().unwrap().into()
    }
}

fn seqs(page: &Value) -> Vec<u64> {
    page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect()
}

fn cursor(page: &Value) -> String {
    page["next_cursor"].as_str().unwrap().into()
}

const BASE: &[Row<'static>] = &[
    (100, "App", Level::Info, "start"),
    (100, "App", Level::Info, "ready"),
];

#[test]
fn mark_accepts_filters_waits_for_matches_and_streams_the_window() {
    let s = Sandbox::new();
    let dir = s.dir();
    s.publish(BASE);
    let mark = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--start-at",
        "end",
    ]);
    let mark = cursor(&mark);
    // A waiting request returns as soon as a matching record is published.
    let publisher = {
        let root = s.root.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let s = Sandbox { root };
            let mut rows = BASE.to_vec();
            rows.push((200, "Net", Level::Warn, "noise"));
            rows.push((200, "Net", Level::Error, "timeout talking to server"));
            s.publish(&rows);
            std::mem::forget(s);
        })
    };
    let started = Instant::now();
    let waited = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        &mark,
        "--min-level",
        "E",
        "--wait-ms",
        "10000",
    ]);
    publisher.join().unwrap();
    assert_eq!(seqs(&waited), [4]);
    assert_eq!(waited["wait"]["stop_reason"], "matched");
    assert!(started.elapsed() < Duration::from_secs(8));
    // The untouched mark still covers the same window with another filter;
    // --mode is implied by the cursor.
    let other = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--cursor",
        &mark,
        "--tag",
        "Net",
    ]);
    assert_eq!(other["mode"], "tail");
    assert_eq!(seqs(&other), [3, 4]);
    // A consumed cursor is bound to its filter again.
    assert_eq!(
        s.fail(&[
            "query",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--cursor",
            &cursor(&other),
            "--tag",
            "App",
        ]),
        "cursor_mismatch"
    );
    // Nothing new: the deadline ends the wait and the cursor stays reusable.
    let idle = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        &cursor(&waited),
        "--min-level",
        "E",
        "--wait-ms",
        "250",
    ]);
    assert!(seqs(&idle).is_empty());
    assert_eq!(idle["wait"]["stop_reason"], "deadline");
    assert!(idle["wait"]["waited_ms"].as_u64().unwrap() >= 200);
    assert_eq!(cursor(&idle), cursor(&waited));
    assert_eq!(
        s.fail(&["query", "--session-dir", &dir, "--wait-ms", "10"]),
        "invalid_arguments"
    );
    // --all in tail mode streams everything after the mark up to the watermark.
    let stream = s.run(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        &mark,
        "--all",
        "--format",
        "text",
    ]);
    assert!(stream.ok, "{}", String::from_utf8_lossy(&stream.stderr));
    let text = String::from_utf8(stream.stdout).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains("timeout talking to server"));
    let summary: Value = serde_json::from_slice(&stream.stderr).unwrap();
    assert_eq!(summary["complete"], true);
    assert_eq!(summary["records_returned"], 2);
}

#[test]
fn waiting_stops_when_the_source_ends() {
    let s = Sandbox::new();
    let dir = s.dir();
    s.publish(BASE);
    let path = s.root.join("session/status.json");
    let mut status: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    status["state"] = "completed".into();
    fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();
    let started = Instant::now();
    let page = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--start-at",
        "end",
        "--wait-ms",
        "5000",
    ]);
    assert_eq!(page["wait"]["stop_reason"], "source_ended");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn package_filter_reports_records_without_process_names() {
    let s = Sandbox::new();
    let dir = s.dir();
    s.publish(BASE);
    let page = s.ok(&["query", "--session-dir", &dir, "--package", "com.example"]);
    assert!(seqs(&page).is_empty());
    assert_eq!(page["warnings"][0]["code"], "package_unresolved");
    assert_eq!(page["warnings"][0]["records"], 2);
    let plain = s.ok(&["query", "--session-dir", &dir]);
    assert!(plain.get("warnings").is_none());
}

#[test]
fn tokens_stay_compact() {
    let s = Sandbox::new();
    let dir = s.dir();
    s.publish(BASE);
    let page = s.ok(&["query", "--session-dir", &dir, "--limit", "1"]);
    let reference = page["records"][0]["record_ref"].as_str().unwrap();
    assert!(reference.starts_with("r3."));
    assert!(reference.len() < 400, "{}", reference.len());
    assert!(cursor(&page).starts_with("c5."));
    assert!(cursor(&page).len() < 800, "{}", cursor(&page).len());
}

#[test]
fn crashes_group_java_native_and_anr_with_drilldown_references() {
    use Level::{Error as E, Fatal as F, Info as I};
    let s = Sandbox::new();
    let dir = s.dir();
    let rows: Vec<Row<'_>> = vec![
        (100, "App", I, "start"),
        (200, "AndroidRuntime", E, "FATAL EXCEPTION: main"),
        (300, "Other", I, "noise"),
        (
            200,
            "AndroidRuntime",
            E,
            "Process: com.example.app, PID: 200",
        ),
        (
            200,
            "AndroidRuntime",
            E,
            "java.lang.RuntimeException: Unable to start",
        ),
        (
            200,
            "AndroidRuntime",
            E,
            "\tat com.example.Main.onCreate(Main.kt:10)",
        ),
        (
            200,
            "AndroidRuntime",
            E,
            "Caused by: java.lang.NullPointerException: boom",
        ),
        (
            200,
            "AndroidRuntime",
            E,
            "\tat com.example.Repo.load(Repo.kt:42)",
        ),
        (201, "AndroidRuntime", E, "FATAL EXCEPTION: main"),
        (
            201,
            "AndroidRuntime",
            E,
            "Process: com.example.app, PID: 201",
        ),
        (
            201,
            "AndroidRuntime",
            E,
            "java.lang.RuntimeException: Unable to start",
        ),
        (
            201,
            "AndroidRuntime",
            E,
            "\tat com.example.Main.onCreate(Main.kt:10)",
        ),
        (
            201,
            "AndroidRuntime",
            E,
            "Caused by: java.lang.NullPointerException: other",
        ),
        (
            201,
            "AndroidRuntime",
            E,
            "\tat com.example.Repo.load(Repo.kt:42)",
        ),
        (
            400,
            "libc",
            F,
            "Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0 in tid 401 (RenderThread), pid 400 (com.example.native)",
        ),
        (
            500,
            "DEBUG",
            F,
            "*** *** *** *** *** *** *** *** *** *** *** ***",
        ),
        (
            500,
            "DEBUG",
            F,
            "pid: 400, tid: 401, name: RenderThread  >>> com.example.native <<<",
        ),
        (
            500,
            "DEBUG",
            F,
            "      #00 pc 000000000004b0c0  /apex/com.android.runtime/lib64/bionic/libc.so (abort+164) (BuildId: abc)",
        ),
        (
            500,
            "DEBUG",
            F,
            "      #01 pc 0000000000012345  /data/app/lib/arm64/libgame.so (Game::tick()+48)",
        ),
        (
            1000,
            "ActivityManager",
            E,
            "ANR in com.example.app (com.example.app/.MainActivity)",
        ),
        (1000, "ActivityManager", E, "PID: 200"),
        (
            1000,
            "ActivityManager",
            E,
            "Reason: Input dispatching timed out (Waited 5001ms)",
        ),
        (
            7,
            "AndroidRuntime",
            E,
            "FATAL EXCEPTION: worker\nProcess: com.example.multi, PID: 7\njava.lang.IllegalStateException: x\n\tat a.B.c(B.kt:1)",
        ),
    ];
    s.publish(&rows);
    let page = s.ok(&["crashes", "--session-dir", &dir]);
    assert_eq!(page["type"], "crashes");
    assert_eq!(page["complete"], true);
    assert_eq!(page["incidents"], 5);
    let groups = page["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 4, "{page:#}");
    let java = &groups[0];
    assert_eq!(java["kind"], "java_crash");
    assert_eq!(
        java["signature"],
        "java.lang.NullPointerException @ com.example.Repo.load(Repo.kt:42)"
    );
    assert_eq!(java["occurrences"], 2);
    assert_eq!(java["pids"], json!([200, 201]));
    assert_eq!(java["process"], "com.example.app");
    assert_eq!(java["thread"], "main");
    assert_eq!(
        java["headline"],
        "java.lang.RuntimeException: Unable to start"
    );
    assert_eq!(java["cause"], "java.lang.NullPointerException: boom");
    assert_eq!(java["first_seq"], 2);
    assert_eq!(java["last_seq"], 14);
    let native = &groups[1];
    assert_eq!(native["kind"], "native_crash");
    assert_eq!(native["signature"], "SIGSEGV @ libgame.so (Game::tick())");
    assert_eq!(native["pids"], json!([400]));
    assert_eq!(native["process"], "com.example.native");
    assert_eq!(native["thread"], "RenderThread");
    assert_eq!(native["frames"].as_array().unwrap().len(), 2);
    let anr = &groups[2];
    assert_eq!(anr["kind"], "anr");
    assert_eq!(
        anr["signature"],
        "ANR in com.example.app: Input dispatching timed out (Waited #ms)"
    );
    assert_eq!(anr["pids"], json!([200]));
    let multi = &groups[3];
    assert_eq!(
        multi["signature"],
        "java.lang.IllegalStateException @ a.B.c(B.kt:1)"
    );
    assert_eq!(multi["process"], "com.example.multi");

    // Drill down from the group's first reference, following only its PID.
    let reference = java["first_ref"].as_str().unwrap();
    let get = s.ok(&["get", "--session-dir", &dir, "--ref", reference]);
    assert_eq!(get["seq"], 2);
    let context = s.ok(&[
        "context",
        "--session-dir",
        &dir,
        "--ref",
        reference,
        "--same-pid",
        "--before",
        "3",
        "--after",
        "6",
    ]);
    assert_eq!(seqs(&context), [2, 4, 5, 6, 7, 8]);
    assert_eq!(context["same_pid"], 200);
    // seq 1 before; seq 3 and 9..=23 after, while looking for a sixth match.
    assert_eq!(context["skipped_other_pid"], 17);
    assert_eq!(context["partial_after"], false);

    let limited = s.ok(&["crashes", "--session-dir", &dir, "--limit", "1"]);
    assert_eq!(limited["groups"].as_array().unwrap().len(), 1);
    assert_eq!(limited["groups_total"], 4);
    assert_eq!(limited["groups_truncated"], true);
    // Crash cursors resume crash scans only.
    assert_eq!(
        s.fail(&["query", "--session-dir", &dir, "--cursor", &cursor(&page)]),
        "cursor_mismatch"
    );
    // A mark scopes the crash scan to records after it.
    let mark = s.ok(&[
        "query",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--start-at",
        "end",
    ]);
    let after = s.ok(&[
        "crashes",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        &cursor(&mark),
    ]);
    assert!(after["groups"].as_array().unwrap().is_empty());
}

#[test]
fn seq_addressing_bisects_without_scanning_history() {
    let s = Sandbox::new();
    let dir = s.dir();
    let messages: Vec<String> = (1..=20_000).map(|i| format!("row-{i}")).collect();
    let rows: Vec<Row<'_>> = messages
        .iter()
        .map(|m| (100, "App", Level::Info, m.as_str()))
        .collect();
    s.publish(&rows);
    let get = s.ok(&[
        "get",
        "--session-dir",
        &dir,
        "--seq",
        "12345",
        "--view",
        "decoded",
    ]);
    assert_eq!(get["seq"], 12345);
    let decoded: Value = serde_json::from_str(get["record_json_chunk"].as_str().unwrap()).unwrap();
    assert_eq!(decoded["message"], "row-12345");
    // Bisection reads a few KiB per step, far less than the ~3 MB history.
    assert!(
        get["file_bytes_read"].as_u64().unwrap() < 256 * 1024,
        "{}",
        get["file_bytes_read"]
    );
    for seq in ["1", "20000"] {
        let edge = s.ok(&["get", "--session-dir", &dir, "--seq", seq]);
        assert_eq!(edge["seq"].to_string(), seq);
    }
    let context = s.ok(&[
        "context",
        "--session-dir",
        &dir,
        "--seq",
        "500",
        "--before",
        "1",
        "--after",
        "1",
    ]);
    assert_eq!(seqs(&context), [499, 500, 501]);
    assert_eq!(
        s.fail(&["get", "--session-dir", &dir, "--seq", "20001"]),
        "record_not_found"
    );
    assert_eq!(
        s.fail(&["get", "--session-dir", &dir, "--seq", "0"]),
        "record_not_found"
    );
    assert_eq!(
        s.fail(&["get", "--session-dir", &dir, "--seq", "x1"]),
        "invalid_arguments"
    );
    let reference = context["records"][0]["record_ref"].as_str().unwrap();
    assert_eq!(
        s.fail(&[
            "get",
            "--session-dir",
            &dir,
            "--seq",
            "1",
            "--ref",
            reference
        ]),
        "invalid_arguments"
    );
}

#[test]
fn skill_command_prints_the_repository_skill() {
    let s = Sandbox::new();
    let output = s.run(&["skill"]);
    assert!(output.ok);
    let expected = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../skills/ciallogcat-cli/SKILL.md"
    ))
    .unwrap();
    assert_eq!(output.stdout, expected);
    assert!(
        output
            .stdout
            .starts_with(b"---\nname: ciallogcat-cli\ndescription: ")
    );
    assert!(!s.run(&["skill", "--extra", "x"]).ok);
}
