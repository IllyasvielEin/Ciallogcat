//! End-to-end query upgrade contract: bounded child lifetime, isolated signing
//! keys, and file-backed outputs so large UTF-8 streams cannot deadlock pipes.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/native_fixture.rs"]
mod native_fixture;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct Sandbox(PathBuf);
struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}
impl Output {
    fn page(&self) -> Value {
        assert!(self.success, "{}", self.stderr);
        assert!(self.stderr.is_empty(), "unexpected JSON-page stderr");
        serde_json::from_str(&self.stdout).unwrap()
    }
    fn summary(&self) -> Value {
        assert!(self.success, "{}", self.stderr);
        let summary: Value = serde_json::from_str(&self.stderr).unwrap();
        assert_eq!(summary["type"], "query_summary");
        assert!(summary.get("records").is_none());
        summary
    }
    fn rows(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Sandbox {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/cli-query-upgrade")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }
    fn file(&self, rows: &[Value]) -> String {
        let path = self.path("logs.jsonl");
        fs::write(&path, native_fixture::bytes(rows, true)).unwrap();
        path
    }
    fn session(&self, rows: &[Value]) -> String {
        let directory = self.path("session");
        fs::create_dir(&directory).unwrap();
        fs::write(
            Path::new(&directory).join("logs.jsonl"),
            native_fixture::bytes(rows, false),
        )
        .unwrap();
        self.publish(
            &directory,
            rows.last().map_or(0, |row| row["seq"].as_u64().unwrap()),
        );
        directory
    }
    fn publish(&self, directory: &str, seq: u64) {
        let bytes = fs::metadata(Path::new(directory).join("logs.jsonl"))
            .unwrap()
            .len();
        let mut status = json!({"schema_version":1,"type":"status","state":"running","updated_unix_ms":u64::MAX});
        native_fixture::set_status(&mut status, seq, bytes);
        fs::write(Path::new(directory).join("status.json"), status.to_string()).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        let id = uuid::Uuid::new_v4();
        let stdout = self.0.join(format!("{id}.out"));
        let stderr = self.0.join(format!("{id}.err"));
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args(args)
                .env("CIALLOGCAT_CONFIG_DIR", self.0.join("config"))
                .stdin(Stdio::null())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "CLI timeout: {args:?}");
            std::thread::sleep(Duration::from_millis(5));
        };
        Output {
            success: status.success(),
            stdout: fs::read_to_string(stdout).unwrap(),
            stderr: fs::read_to_string(stderr).unwrap(),
        }
    }
    fn fail(&self, args: &[&str], code: &str) {
        let output = self.run(args);
        assert!(!output.success, "unexpected success: {}", output.stdout);
        assert!(output.stdout.is_empty(), "invalid request wrote records");
        let error: Value = serde_json::from_str(&output.stderr).unwrap();
        assert_eq!(error["error"]["code"], code, "{error}");
    }
}
fn row(seq: u64, message: &str) -> Value {
    native_fixture::row(seq, message, None)
}

fn seqs(page: &Value) -> Vec<u64> {
    page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect()
}

#[test]
fn all_crosses_empty_and_sparse_scan_pages_until_explicit_completion() {
    let sandbox = Sandbox::new();
    let rows: Vec<_> = (1..=9)
        .map(|seq| row(seq, if seq == 8 { "needle" } else { "other" }))
        .collect();
    let file = sandbox.file(&rows);
    let page = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--query",
            "needle",
            "--max-scan-records",
            "2",
        ])
        .page();
    assert!(seqs(&page).is_empty());
    assert_eq!(page["complete"], false);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["scan"]["stop_reason"], "scan_budget");
    let output = sandbox.run(&[
        "query",
        "--file",
        &file,
        "--query",
        "needle",
        "--max-scan-records",
        "2",
        "--all",
    ]);
    let summary = output.summary();
    assert_eq!(output.rows(), vec![rows[7].clone()]);
    assert_eq!(summary["complete"], true);
    assert_eq!(summary["has_more"], false);
    assert_eq!(summary["total_scanned_records"], 9);
    assert!(summary["pages"].as_u64().unwrap() >= 5);
    let empty = sandbox.run(&[
        "query",
        "--file",
        &file,
        "--query",
        "absent",
        "--max-scan-records",
        "1",
        "--all",
    ]);
    assert!(empty.stdout.is_empty());
    assert_eq!(empty.summary()["total_scanned_records"], 9);
    assert_eq!(empty.summary()["complete"], true);
}

#[test]
fn full_utf8_streams_ignore_preview_and_response_sizes() {
    let sandbox = Sandbox::new();
    let message = "雪🌟\\\"\n".repeat(20_000);
    let original = row(1, &message);
    let file = sandbox.file(std::slice::from_ref(&original));
    let output = sandbox.run(&[
        "query",
        "--file",
        &file,
        "--all",
        "--preview-bytes",
        "0",
        "--max-response-bytes",
        "8192",
    ]);
    assert_eq!(output.rows(), vec![original]);
    assert_eq!(output.summary()["records_returned"], 1);
    let text = sandbox.run(&[
        "query",
        "--file",
        &file,
        "--all",
        "--format",
        "text",
        "--preview-bytes",
        "0",
        "--max-response-bytes",
        "8192",
    ]);
    assert!(text.stdout.contains(&message), "text output was truncated");
    assert_eq!(text.summary()["complete"], true);
}

#[test]
fn one_page_stream_has_only_records_on_stdout_and_resumable_stderr() {
    let sandbox = Sandbox::new();
    let rows = [row(1, "first"), row(2, "second")];
    let file = sandbox.file(&rows);
    let first = sandbox.run(&[
        "query", "--file", &file, "--format", "jsonl", "--limit", "1",
    ]);
    assert_eq!(first.rows(), vec![rows[0].clone()]);
    let summary = first.summary();
    assert_eq!(summary["complete"], false);
    let second = sandbox.run(&[
        "query",
        "--file",
        &file,
        "--format",
        "jsonl",
        "--cursor",
        summary["next_cursor"].as_str().unwrap(),
    ]);
    assert_eq!(second.rows(), vec![rows[1].clone()]);
    assert_eq!(second.summary()["complete"], true);
}

#[test]
fn snapshot_cursor_does_not_expand_after_file_append_and_binds_filters() {
    let sandbox = Sandbox::new();
    let directory = sandbox.session(&[row(1, "Match"), row(2, "MATCH")]);
    let file = Path::new(&directory).join("logs.jsonl");
    let args = [
        "query",
        "--session-dir",
        &directory,
        "--query",
        "^match$",
        "--regex",
        "--ignore-case",
        "--tag",
        "App",
        "--limit",
        "1",
    ];
    let first = sandbox.run(&args).page();
    assert_eq!(seqs(&first), [1]);
    writeln!(
        OpenOptions::new().append(true).open(&file).unwrap(),
        "{}",
        row(3, "match")
    )
    .unwrap();
    let cursor = first["next_cursor"].as_str().unwrap();
    let mut next = args.to_vec();
    next.extend(["--cursor", cursor]);
    let second = sandbox.run(&next).page();
    assert_eq!(seqs(&second), [2]);
    assert_eq!(second["complete"], true);
    let mut mismatch = next.clone();
    mismatch[6] = "--case-sensitive";
    sandbox.fail(&mismatch, "cursor_mismatch");
    sandbox.publish(&directory, 3);
    let fresh = sandbox.run(&args[..args.len() - 2]).page();
    assert_eq!(seqs(&fresh), [1, 2, 3]);
}

#[test]
fn message_case_tag_pid_tid_and_level_predicates_combine() {
    let sandbox = Sandbox::new();
    let mut rows = vec![
        row(1, "Error 42"),
        row(2, "error 42"),
        row(3, "error 42"),
        row(4, "error 42"),
        row(5, "error 42"),
    ];
    rows[2]["tag"] = "Other".into();
    rows[3]["pid"] = 99.into();
    rows[4]["level"] = "D".into();
    let file = sandbox.file(&rows);
    let literal = sandbox
        .run(&["query", "--file", &file, "--query", "Error"])
        .page();
    assert_eq!(seqs(&literal), [1]);
    let filtered = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--query",
            "^error [0-9]+$",
            "--regex",
            "--ignore-case",
            "--tag",
            "App",
            "--tag-exclude",
            "Other",
            "--pid",
            "42",
            "--tid",
            "43",
            "--min-level",
            "info",
        ])
        .page();
    assert_eq!(seqs(&filtered), [1, 2]);
    let excluded = sandbox
        .run(&["query", "--file", &file, "--tag-exclude", "App"])
        .page();
    assert_eq!(seqs(&excluded), [3]);
}

#[test]
fn invalid_regex_incompatible_modes_and_missing_time_fail_explicitly() {
    let sandbox = Sandbox::new();
    let file = sandbox.file(&[row(1, "anything")]);
    for flags in [
        vec!["--query", "[", "--regex"],
        vec!["--all", "--format", "json"],
        vec!["--all", "--mode", "tail"],
        vec!["--ignore-case", "--case-sensitive"],
        vec!["--pid", "-1"],
        vec!["--tag", "a", "--tag-include", "b"],
    ] {
        let mut args = vec!["query", "--file", &file];
        args.extend(flags);
        sandbox.fail(&args, "invalid_arguments");
    }
    sandbox.fail(
        &["query", "--file", &file, "--since", "2026-01-01T00:00:00Z"],
        "time_unavailable",
    );
    sandbox.fail(
        &[
            "query",
            "--file",
            &file,
            "--since",
            "2026-01-01T00:00:00Z",
            "--time-basis",
            "device",
        ],
        "time_unavailable",
    );
}

fn native_file(sandbox: &Sandbox, observed: &[u64]) -> String {
    use ciallogcat::document::{self, CaptureStatus, Document, DocumentHeader, ExportScope};
    use ciallogcat::log_store::LogStore;
    use ciallogcat::model::{Level, LogBuffer, LogEntry};
    use ciallogcat::session::Session;
    use std::sync::{Arc, atomic::AtomicBool};
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut store = LogStore::default();
    for &time in observed {
        let mut entry = LogEntry::new(
            "12-31 23:59:59.000",
            42,
            43,
            "AppTag",
            Level::Info,
            "observed",
        );
        entry.provenance = session.next_record(Some(time));
        store.push(entry);
    }
    let document = Document::from_snapshot(
        DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]),
        Arc::new(store),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap();
    let file = sandbox.path("native.jsonl");
    document::write_snapshot(
        File::create(&file).unwrap(),
        &document,
        &AtomicBool::new(false),
    )
    .unwrap();
    file
}

#[test]
fn received_time_range_is_inclusive_exclusive_and_timezone_exact_across_year() {
    let sandbox = Sandbox::new();
    // 2026-01-01T00:00:00Z; deliberately unrelated yearless display timestamps.
    let midnight = 1_767_225_600_000;
    let file = native_file(&sandbox, &[midnight - 1, midnight, midnight + 1]);
    let page = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--since",
            "2026-01-01T01:00:00+01:00",
            "--until",
            "2026-01-01T00:00:00.001Z",
        ])
        .page();
    assert_eq!(seqs(&page), [2]);
    assert_eq!(page["complete"], true);
    sandbox.fail(
        &[
            "query",
            "--file",
            &file,
            "--since",
            "2026-01-01T00:00:00-00:00",
        ],
        "invalid_arguments",
    );
    sandbox.fail(
        &["query", "--file", &file, "--since", "2026-02-29T00:00:00Z"],
        "invalid_arguments",
    );
}

#[test]
fn relative_time_cursor_reuses_first_page_anchor() {
    let sandbox = Sandbox::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let file = native_file(&sandbox, &[now - 500, now - 400]);
    let first = sandbox
        .run(&["query", "--file", &file, "--since", "30s", "--limit", "1"])
        .page();
    assert_eq!(seqs(&first), [1]);
    std::thread::sleep(Duration::from_millis(20));
    let second = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--since",
            "30s",
            "--cursor",
            first["next_cursor"].as_str().unwrap(),
        ])
        .page();
    assert_eq!(seqs(&second), [2]);
    assert_eq!(first["time_anchor_unix_ms"], second["time_anchor_unix_ms"]);
    assert_eq!(first["time_window"], second["time_window"]);
    assert_eq!(second["complete"], true);
}

#[test]
fn protobuf_device_time_filters_preserve_nanosecond_boundaries_in_native_records() {
    use ciallogcat::document::{self, CaptureStatus, Document, DocumentHeader, ExportScope};
    use ciallogcat::log_store::LogStore;
    use ciallogcat::model::LogBuffer;
    use ciallogcat::session::Session;
    use std::sync::{Arc, atomic::AtomicBool};
    let sandbox = Sandbox::new();
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut store = LogStore::default();
    for nanos in 1..=3 {
        let mut payload =
            b"\x08\x01\x10\x01\x18\x04\x28\x2a\x30\x2b\x3a\x03Tag\x42\x03msg".to_vec();
        payload[3] = nanos;
        let mut entry = ciallogcat::protobuf::DecodedEntry::decode_payload(payload)
            .unwrap()
            .project();
        entry.provenance = session.next_record(Some(99_000));
        store.push(entry);
    }
    let mut header = DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]);
    header.source.transport_format = Some("logcat_proto_experimental".into());
    let mut document = Document::from_snapshot(
        header,
        Arc::new(store),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap();
    let future = json!({"nested":["雪", 42, null]});
    for metadata in &mut document.record_metadata {
        metadata.extra.insert("future".into(), future.clone());
        metadata.proto_extra.insert("future".into(), future.clone());
        metadata
            .provenance_extra
            .insert("future".into(), future.clone());
    }
    let file = sandbox.path("proto.jsonl");
    document::write_snapshot(
        File::create(&file).unwrap(),
        &document,
        &AtomicBool::new(false),
    )
    .unwrap();
    let page = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--time-basis",
            "device",
            "--since",
            "1970-01-01T00:00:01.000000002Z",
            "--until",
            "1970-01-01T00:00:01.000000003Z",
        ])
        .page();
    assert_eq!(seqs(&page), [2]);
    assert_eq!(page["complete"], true);
    // The same numeric range does not match host receive-time provenance.
    let received = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--since",
            "1970-01-01T00:00:01Z",
            "--until",
            "1970-01-01T00:00:02Z",
        ])
        .page();
    assert!(seqs(&received).is_empty());

    let stream = sandbox.run(&[
        "query",
        "--file",
        &file,
        "--all",
        "--format",
        "jsonl",
        "--max-scan-records",
        "1",
    ]);
    assert_eq!(stream.summary()["records_returned"], 3);
    assert_eq!(stream.summary()["complete"], true);
    let streamed = stream.rows();
    let references = sandbox.run(&["query", "--file", &file]).page();
    for (index, streamed) in streamed.iter().enumerate() {
        let decoded = sandbox
            .run(&[
                "get",
                "--file",
                &file,
                "--ref",
                references["records"][index]["record_ref"].as_str().unwrap(),
                "--view",
                "decoded",
            ])
            .page();
        assert_eq!(decoded["complete"], true);
        let decoded: Value =
            serde_json::from_str(decoded["record_json_chunk"].as_str().unwrap()).unwrap();
        assert_eq!(*streamed, decoded);
        assert_eq!(streamed["future"], future);
        assert_eq!(streamed["proto"]["future"], future);
        assert_eq!(streamed["provenance"]["future"], future);
        assert!(streamed["proto"]["raw_payload"].is_string());
        assert!(streamed["proto"]["message_bytes"].is_string());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn capture_default_is_one_gib_disk_cap_not_an_unbounded_duration() {
    let sandbox = Sandbox::new();
    let directory = sandbox.path("session");
    // A successful empty fake executable is sufficient to exercise persisted
    // configuration; no real ADB or device access occurs.
    let start = sandbox.run(&[
        "start",
        "--serial",
        "TEST",
        "--adb",
        "/bin/true",
        "--session-dir",
        &directory,
        "--dump",
    ]);
    let config: Value =
        serde_json::from_slice(&fs::read(Path::new(&directory).join("config.json")).unwrap())
            .unwrap();
    let stop = sandbox.run(&["stop", "--session-dir", &directory, "--wait-secs", "10"]);
    assert!(stop.success, "start={} stop={}", start.stderr, stop.stderr);
    assert_eq!(config["max_bytes"], 1_073_741_824u64);
    assert_eq!(config["duration_secs"], 600);
    assert_eq!(config["max_entries"], 10_000_000);
}

#[test]
fn late_stream_validation_failure_keeps_prefix_but_never_emits_success_summary() {
    let sandbox = Sandbox::new();
    let first = row(1, "valid prefix");
    let directory = sandbox.session(std::slice::from_ref(&first));
    let file = Path::new(&directory).join("logs.jsonl");
    writeln!(
        OpenOptions::new().append(true).open(&file).unwrap(),
        "not JSON"
    )
    .unwrap();
    sandbox.publish(&directory, 2);
    let output = sandbox.run(&[
        "query",
        "--session-dir",
        &directory,
        "--all",
        "--format",
        "jsonl",
        "--max-scan-records",
        "1",
    ]);
    assert!(!output.success);
    assert_eq!(output.rows(), vec![first]);
    let error: Value = serde_json::from_str(&output.stderr).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["code"], "invalid_source");
    assert!(error.get("complete").is_none());
    assert!(!output.stderr.contains("query_summary"));
}

#[test]
fn native_received_time_filters_preserve_capture_identity() {
    let sandbox = Sandbox::new();
    let midnight = 1_767_225_600_000u64;
    let rows: Vec<_> = (0..3)
        .map(|i| {
            let mut record = row(i + 1, "native observed");
            record["provenance"]["observed_unix_ms"] = (midnight + i).into();
            record
        })
        .collect();
    let file = sandbox.file(&rows);
    let page = sandbox
        .run(&[
            "query",
            "--file",
            &file,
            "--since",
            "2026-01-01T00:00:00.001Z",
            "--until",
            "2026-01-01T00:00:00.002Z",
        ])
        .page();
    assert_eq!(seqs(&page), [2]);
    assert_eq!(page["records"][0]["identity"], rows[1]["provenance"]);
    assert_eq!(page["records"][0]["identity_scope"], "capture_session");
    let reference = page["records"][0]["record_ref"].as_str().unwrap();
    let get = sandbox
        .run(&[
            "get", "--file", &file, "--ref", reference, "--view", "decoded",
        ])
        .page();
    assert_eq!(get["identity"], rows[1]["provenance"]);
    let decoded: Value = serde_json::from_str(get["record_json_chunk"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, rows[1]);
    assert_eq!(decoded["provenance"], rows[1]["provenance"]);
    assert!(decoded.get("session_id").is_none());
    // Optional receive time does not turn yearless text into an exact device clock.
    sandbox.fail(
        &[
            "query",
            "--file",
            &file,
            "--since",
            "2026-01-01T00:00:00Z",
            "--time-basis",
            "device",
        ],
        "time_unavailable",
    );
}

#[test]
fn native_received_time_and_unknown_fields_survive_every_query_view() {
    let sandbox = Sandbox::new();
    let mut original = row(1, "native extensions 雪");
    original["provenance"]["observed_unix_ms"] = 1_767_225_600_123u64.into();
    original["future"] = json!({"nested":["雪",null,true], "precise":18446744073709551615u64});
    let file = sandbox.file(std::slice::from_ref(&original));
    let page = sandbox.run(&["query", "--file", &file]).page();
    let reference = page["records"][0]["record_ref"].as_str().unwrap();
    for view in ["stored", "decoded"] {
        let get = sandbox
            .run(&["get", "--file", &file, "--ref", reference, "--view", view])
            .page();
        assert_eq!(get["complete"], true);
        let record: Value =
            serde_json::from_str(get["record_json_chunk"].as_str().unwrap()).unwrap();
        assert_eq!(record, original, "{view}");
        assert_eq!(get["identity"], original["provenance"]);
    }
    let stream = sandbox.run(&["query", "--file", &file, "--all"]);
    assert_eq!(stream.rows(), vec![original.clone()]);
    assert_eq!(stream.summary()["complete"], true);
    let directory = sandbox.session(std::slice::from_ref(&original));
    let page = sandbox.run(&["query", "--session-dir", &directory]).page();
    assert_eq!(page["schema_version"], 2);
    assert_eq!(page["source_schema_version"], 3);
    assert_eq!(
        page["records"][0]["observed_unix_ms"],
        original["provenance"]["observed_unix_ms"]
    );
    let stream = sandbox.run(&["query", "--session-dir", &directory, "--all"]);
    assert_eq!(stream.rows(), vec![original]);
    assert_eq!(stream.summary()["complete"], true);
}

#[test]
fn malformed_native_receive_timestamps_are_invalid_sources_not_missing_times() {
    for invalid in [
        json!(-1),
        json!("1767225600000"),
        serde_json::from_str::<Value>("18446744073709551616").unwrap(),
    ] {
        let sandbox = Sandbox::new();
        let mut original = row(1, "invalid clock");
        original["provenance"]["observed_unix_ms"] = invalid;
        let file = sandbox.file(&[original]);
        sandbox.fail(&["query", "--file", &file], "invalid_source");
        sandbox.fail(
            &["query", "--file", &file, "--since", "2026-01-01T00:00:00Z"],
            "invalid_source",
        );
    }
}

#[test]
fn relative_empty_results_explain_resolved_window_without_inventing_coverage() {
    let s = Sandbox::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut record = row(1, "ninety seconds old");
    record["provenance"]["observed_unix_ms"] = (now - 90_000).into();
    let file = s.file(&[record]);
    for (since, seconds, expected) in [("60s", 60i128, 0usize), ("2m", 120, 1)] {
        for stream in [false, true] {
            let mut args = vec!["query", "--file", &file, "--since", since];
            if stream {
                args.extend(["--all", "--format", "jsonl"]);
            }
            let output = s.run(&args);
            let result = if stream {
                output.summary()
            } else {
                output.page()
            };
            assert_eq!(result["complete"], true);
            if stream {
                assert_eq!(result["records_returned"], expected);
            } else {
                assert_eq!(result["records"].as_array().unwrap().len(), expected);
            }
            let window = &result["time_window"];
            assert_eq!(window["basis"], "received");
            let anchor = result["time_anchor_unix_ms"].as_u64().unwrap();
            assert_eq!(window["anchor_unix_ms"], anchor);
            assert_eq!(
                window["since_inclusive"]["unix_ns"],
                (i128::from(anchor) * 1_000_000 - seconds * 1_000_000_000).to_string()
            );
            let resolved = window["since_inclusive"]["rfc3339"].as_str().unwrap();
            assert!(resolved.ends_with('Z'), "{resolved}");
            assert!(window["until_exclusive"].is_null());
        }
    }
    let unfiltered = s.run(&["query", "--file", &file]).page();
    assert_eq!(
        unfiltered["time_window"],
        json!({"basis":"received","anchor_unix_ms":null,"since_inclusive":null,"until_exclusive":null})
    );
}

#[test]
fn absolute_window_metadata_preserves_nanoseconds_and_exclusive_end() {
    let s = Sandbox::new();
    let mut record = row(1, "outside nanosecond interval");
    record["provenance"]["observed_unix_ms"] = 1000.into();
    let file = s.file(&[record]);
    let page = s
        .run(&[
            "query",
            "--file",
            &file,
            "--since",
            "1970-01-01T01:00:01.000000002+01:00",
            "--until",
            "1970-01-01T00:00:01.000000003Z",
        ])
        .page();
    assert!(page["records"].as_array().unwrap().is_empty());
    assert_eq!(
        page["time_window"]["since_inclusive"],
        json!({"unix_ns":"1000000002","rfc3339":"1970-01-01T00:00:01.000000002Z"})
    );
    assert_eq!(
        page["time_window"]["until_exclusive"],
        json!({"unix_ns":"1000000003","rfc3339":"1970-01-01T00:00:01.000000003Z"})
    );
}

#[test]
fn structural_records_share_literal_filter_but_never_fabricate_log_metadata() {
    use ciallogcat::model::{Level, LogEntry};
    let s = Sandbox::new();
    let rows = [
        native_fixture::entry_row(
            1,
            LogEntry::new("now", 0, 0, "Tag", Level::Verbose, "shared log"),
            None,
        ),
        native_fixture::entry_row(2, LogEntry::marker("shared event"), None),
        native_fixture::entry_row(3, LogEntry::unparsed("shared raw"), None),
    ];
    let file = s.file(&rows);
    let all = s
        .run(&["query", "--file", &file, "--query", "shared"])
        .page();
    assert_eq!(seqs(&all), [1, 2, 3]);
    let verbose = s
        .run(&[
            "query",
            "--file",
            &file,
            "--query",
            "shared",
            "--min-level",
            "V",
        ])
        .page();
    assert_eq!(seqs(&verbose), [1, 2, 3]);
    for record in &all["records"].as_array().unwrap()[1..] {
        for field in ["pid", "tid", "level", "tag", "process"] {
            assert!(record.get(field).is_none(), "{record}");
        }
    }
    for flags in [
        vec!["--tag-exclude", "Other"],
        vec!["--tag", "Tag"],
        vec!["--pid", "0"],
        vec!["--tid", "0"],
    ] {
        let mut args = vec!["query", "--file", &file];
        args.extend(flags);
        assert_eq!(seqs(&s.run(&args).page()), [1]);
    }
}
