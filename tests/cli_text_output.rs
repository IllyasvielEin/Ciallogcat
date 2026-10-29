//! CLI-only presentation contracts. Files/JSONL retain evidence; display clocks
//! and line terminators must not modify archives or invent timestamp provenance.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use ciallogcat::document::{self, CaptureStatus, Document, DocumentHeader, ExportScope};
use ciallogcat::log_store::LogStore;
use ciallogcat::model::{LogBuffer, LogEntry};
use ciallogcat::session::Session;
use serde_json::Value;

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
    fn summary(&self) -> Value {
        assert!(self.success, "{}", self.stderr);
        let summary: Value = serde_json::from_str(&self.stderr).unwrap();
        assert_eq!(summary["type"], "query_summary");
        summary
    }
    fn page(&self) -> Value {
        assert!(self.success, "{}", self.stderr);
        assert!(self.stderr.is_empty());
        serde_json::from_str(&self.stdout).unwrap()
    }
}
impl Sandbox {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/cli-text-output")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn filename(&self) -> String {
        self.0
            .join(format!("{}.jsonl", uuid::Uuid::new_v4()))
            .to_str()
            .unwrap()
            .to_owned()
    }
    fn records(&self, records: &[Value]) -> String {
        let path = self.filename();
        fs::write(&path, native_fixture::bytes(records, true)).unwrap();
        path
    }
    fn native(&self, entries: Vec<(LogEntry, Option<u64>)>) -> String {
        let mut session = Session::new();
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        let mut store = LogStore::default();
        for (mut entry, received) in entries {
            entry.provenance = session.next_record(received);
            store.push(entry);
        }
        let mut header = DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]);
        header.source.transport_format = Some("logcat_proto_experimental".into());
        let document = Document::from_snapshot(
            header,
            Arc::new(store),
            ExportScope::default(),
            CaptureStatus::default(),
        )
        .unwrap();
        let path = self.filename();
        document::write_snapshot(
            File::create(&path).unwrap(),
            &document,
            &AtomicBool::new(false),
        )
        .unwrap();
        path
    }
    fn run(&self, args: &[&str], timezone: Option<&str>) -> Output {
        let id = uuid::Uuid::new_v4();
        let out = self.0.join(format!("{id}.out"));
        let err = self.0.join(format!("{id}.err"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"));
        command
            .args(args)
            .env("CIALLOGCAT_CONFIG_DIR", self.0.join("config"))
            .stdin(Stdio::null())
            .stdout(File::create(&out).unwrap())
            .stderr(File::create(&err).unwrap());
        if let Some(timezone) = timezone {
            command.env("TZ", timezone);
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "CLI timeout: {args:?}");
            std::thread::sleep(Duration::from_millis(5));
        };
        Output {
            success: status.success(),
            stdout: fs::read_to_string(out).unwrap(),
            stderr: fs::read_to_string(err).unwrap(),
        }
    }
    fn text(&self, file: &str, flags: &[&str]) -> Output {
        let mut args = vec!["query", "--file", file, "--all", "--format", "text"];
        args.extend_from_slice(flags);
        let output = self.run(&args, None);
        assert_eq!(output.summary()["complete"], true);
        output
    }
}
fn row(seq: u64, message: &str, received: Option<u64>) -> Value {
    let mut value = native_fixture::row(seq, message, received);
    value["tag"] = "Tag".into();
    value
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
fn proto(message: &str, seconds: u64, nanos: u64) -> LogEntry {
    proto_named(message, seconds, nanos, None)
}
fn proto_named(message: &str, seconds: u64, nanos: u64, process: Option<&str>) -> LogEntry {
    let mut bytes = Vec::new();
    for (field, value) in [(8, seconds), (16, nanos), (24, 4), (40, 42), (48, 43)] {
        bytes.push(field);
        varint(value, &mut bytes);
    }
    for (field, value) in [(58, "Tag"), (66, message)] {
        bytes.push(field);
        varint(value.len() as u64, &mut bytes);
        bytes.extend_from_slice(value.as_bytes());
    }
    if let Some(process) = process {
        bytes.push(74); // process_name, protobuf field 9, length-delimited
        varint(process.len() as u64, &mut bytes);
        bytes.extend_from_slice(process.as_bytes());
    }
    ciallogcat::protobuf::DecodedEntry::decode_payload(bytes)
        .unwrap()
        .project()
}

#[test]
fn default_readable_and_explicit_formats_use_received_milliseconds() {
    let s = Sandbox::new();
    let file = s.records(&[row(1, "payload", Some(1_767_225_600_123))]);
    for (format, prefix) in [
        (None, "2026-01-01 00:00:00.123+00:00"),
        (Some("readable"), "2026-01-01 00:00:00.123+00:00"),
        (Some("rfc3339"), "2026-01-01T00:00:00.123Z"),
        (Some("epoch"), "1767225600.123"),
    ] {
        let mut flags = vec!["--timezone", "utc"];
        if let Some(format) = format {
            flags.extend(["--time-format", format]);
        }
        let output = s.text(&file, &flags);
        assert!(output.stdout.starts_with(prefix), "{}", output.stdout);
        assert!(output.stdout.ends_with("Tag: payload\n"));
        assert!(!output.stdout.contains("[received]"));
        assert_eq!(output.summary()["time_window"]["basis"], "received");
    }
}

#[test]
fn received_and_device_display_follow_the_same_basis_as_time_filters_in_native_records() {
    let s = Sandbox::new();
    let file = s.native(vec![(proto("chosen", 1, 123), Some(99_321))]);
    for (basis, since, until, prefix) in [
        (
            "received",
            "1970-01-01T00:01:39Z",
            "1970-01-01T00:01:40Z",
            "99.321",
        ),
        (
            "device",
            "1970-01-01T00:00:01.000000123Z",
            "1970-01-01T00:00:01.000000124Z",
            "1.000000123 [device]",
        ),
    ] {
        let text = s.text(
            &file,
            &[
                "--time-format",
                "epoch",
                "--time-basis",
                basis,
                "--since",
                since,
                "--until",
                until,
            ],
        );
        assert!(text.stdout.starts_with(prefix), "{}", text.stdout);
        assert_eq!(text.summary()["records_returned"], 1);
        assert_eq!(text.summary()["time_window"]["basis"], basis);
    }
    let device = s.text(
        &file,
        &[
            "--time-basis",
            "device",
            "--timezone",
            "utc",
            "--time-format",
            "rfc3339",
        ],
    );
    assert!(
        device
            .stdout
            .starts_with("1970-01-01T00:00:01.000000123Z [device]")
    );
}

#[test]
fn message_terminators_are_preserved_without_an_extra_blank_line_across_all_schemas() {
    for message in [
        "payload",
        "payload\n",
        "payload\r\n",
        "payload\n\n",
        "payload\ninside\n",
        "",
        "payload  ",
        "payload\r",
    ] {
        let s = Sandbox::new();
        let mut files = vec![s.records(&[row(1, message, Some(0))])];
        {
            files.push(s.native(vec![(proto(message, 1, 0), Some(0))]));
        }
        let expected = if message.ends_with('\n') {
            message.to_owned()
        } else {
            format!("{message}\n")
        };
        for file in files {
            let text = s.text(&file, &["--timezone", "utc"]);
            let rendered = text.stdout.split_once("Tag: ").unwrap().1;
            assert_eq!(rendered, expected, "file {file}");
            let jsonl = s.run(
                &["query", "--file", &file, "--all", "--format", "jsonl"],
                None,
            );
            assert_eq!(jsonl.summary()["records_returned"], 1);
            let value: Value = serde_json::from_str(&jsonl.stdout).unwrap();
            assert_eq!(value["message"], message, "JSONL message changed");
        }
    }
}

#[test]
fn event_and_unparsed_text_do_not_expose_log_placeholders_or_add_terminators() {
    for (entry, marker, expected) in [
        (LogEntry::marker("event\n"), "[event]", "event\n"),
        (
            LogEntry::unparsed("unknown\r\n"),
            "[unparsed]",
            "unknown\r\n",
        ),
    ] {
        let s = Sandbox::new();
        let file = s.native(vec![(entry, Some(1000))]);
        let text = s.text(&file, &["--timezone", "utc"]);
        assert!(text.stdout.contains(marker), "{}", text.stdout);
        assert!(text.stdout.ends_with(expected));
        assert!(!text.stdout.ends_with(&format!("{expected}\n")));
        assert!(!text.stdout.contains("logcat:"));
        assert!(!text.stdout.contains("    0     0 I"));
    }
}

#[test]
fn missing_native_receipt_time_and_calendar_overflow_are_explicit_without_false_epoch() {
    let s = Sandbox::new();
    let old = s.records(&[row(1, "old", None)]);
    for format in ["readable", "rfc3339", "epoch"] {
        let text = s.text(&old, &["--time-format", format, "--timezone", "utc"]);
        assert!(
            text.stdout
                .starts_with("[received-time-unavailable; device-text=\"10-28 00:00:00.000\"]"),
            "{}",
            text.stdout
        );
        assert!(!text.stdout.contains("1970-"));
    }
    let text = s.text(&old, &["--time-basis", "device"]);
    assert!(
        text.stdout
            .starts_with("[device-time-unavailable; device-text=")
    );
    let huge = s.native(vec![(proto("huge", u64::MAX, 1), Some(u64::MAX))]);
    let epoch = s.text(&huge, &["--time-basis", "device", "--time-format", "epoch"]);
    assert!(
        epoch
            .stdout
            .starts_with("18446744073709551615.000000001 [device]")
    );
    let received = s.text(&huge, &["--time-format", "epoch"]);
    assert!(received.stdout.starts_with("18446744073709551.615"));
    for calendar in ["readable", "rfc3339"] {
        let fallback = s.text(
            &huge,
            &[
                "--time-basis",
                "device",
                "--time-format",
                calendar,
                "--timezone",
                "utc",
            ],
        );
        assert!(
            fallback
                .stdout
                .starts_with("18446744073709551615.000000001 [device; calendar-unavailable]"),
            "{}",
            fallback.stdout
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn local_default_observes_dst_and_utc_override_and_epoch_ignore_local_zone() {
    let s = Sandbox::new();
    let file = s.records(&[
        row(1, "before", Some(1_772_953_199_999)),
        row(2, "after", Some(1_772_953_200_000)),
    ]);
    let base = ["query", "--file", &file, "--all", "--format", "text"];
    let local = s.run(&base, Some("America/New_York"));
    assert!(local.success, "{}", local.stderr);
    let lines: Vec<_> = local.stdout.lines().collect();
    assert!(
        lines[0].starts_with("2026-03-08 01:59:59.999-05:00"),
        "{}",
        local.stdout
    );
    assert!(
        lines[1].starts_with("2026-03-08 03:00:00.000-04:00"),
        "{}",
        local.stdout
    );
    let mut utc = base.to_vec();
    utc.extend(["--timezone", "utc"]);
    let utc = s.run(&utc, Some("America/New_York"));
    assert!(utc.stdout.starts_with("2026-03-08 06:59:59.999+00:00"));
    let mut epoch = base.to_vec();
    epoch.extend(["--time-format", "epoch"]);
    let local_epoch = s.run(&epoch, Some("America/New_York"));
    epoch.extend(["--timezone", "utc"]);
    let utc_epoch = s.run(&epoch, Some("America/New_York"));
    assert_eq!(local_epoch.stdout, utc_epoch.stdout);
    assert!(local_epoch.stdout.starts_with("1772953199.999"));
}

#[test]
fn display_options_do_not_change_cursor_filters_or_archived_jsonl() {
    let s = Sandbox::new();
    let original = [
        row(1, "first\n", Some(1000)),
        row(2, "second\r\n", Some(2000)),
    ];
    let file = s.records(&original);
    let bytes_before = fs::read(&file).unwrap();
    let first = s.run(
        &[
            "query",
            "--file",
            &file,
            "--format",
            "text",
            "--limit",
            "1",
            "--time-format",
            "epoch",
        ],
        None,
    );
    let summary = first.summary();
    assert_eq!(summary["complete"], false);
    let second = s.run(
        &[
            "query",
            "--file",
            &file,
            "--format",
            "text",
            "--cursor",
            summary["next_cursor"].as_str().unwrap(),
            "--time-format",
            "rfc3339",
            "--process-width",
            "0",
            "--timezone",
            "utc",
        ],
        None,
    );
    assert_eq!(second.summary()["complete"], true);
    assert!(second.stdout.starts_with("1970-01-01T00:00:02.000Z"));
    let jsonl = s.run(
        &["query", "--file", &file, "--all", "--format", "jsonl"],
        None,
    );
    let records: Vec<Value> = jsonl
        .stdout
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(records, original);
    assert_eq!(fs::read(&file).unwrap(), bytes_before);
    let page = s.run(&["query", "--file", &file], None).page();
    assert_eq!(page["records"][0]["time"], "10-28 00:00:00.000");
}

#[test]
fn invalid_and_non_text_presentation_options_fail_without_stdout() {
    let s = Sandbox::new();
    let file = s.records(&[row(1, "payload", Some(0))]);
    for options in [
        vec!["--format", "text", "--time-format", "unknown"],
        vec!["--format", "text", "--timezone", "America/New_York"],
        vec!["--format", "jsonl", "--time-format", "epoch"],
        vec!["--format", "json", "--timezone", "utc"],
        vec!["--format", "jsonl", "--process-width", "32"],
        vec!["--format", "text", "--process-width", "-1"],
        vec!["--format", "text", "--process-width", "4097"],
    ] {
        let mut args = vec!["query", "--file", &file];
        args.extend(options);
        let output = s.run(&args, None);
        assert!(!output.success, "unexpected success: {}", output.stdout);
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_str(&output.stderr).unwrap();
        assert_eq!(error["error"]["code"], "invalid_arguments");
    }
}

#[test]
fn chromium_process_fixture_is_compact_in_redirected_text_but_lossless_in_jsonl() {
    // Synthetic Chromium-style sandbox name, not a claim of real-device capture.
    let name = "com.example.browser:org.chromium.content.app.SandboxedProcessService0:renderer42";
    let s = Sandbox::new();
    let entry = proto_named("entire message must remain visible\n", 1, 0, Some(name));
    let file = s.native(vec![(entry, Some(1000))]);
    let before = fs::read(&file).unwrap();
    let compact = s.text(&file, &["--time-format", "epoch"]);
    assert!(!compact.stdout.contains(name));
    assert!(compact.stdout.contains('…'));
    assert!(compact.stdout.contains("renderer42"));
    assert!(compact.stdout.contains("   42    43 I "));
    assert!(
        compact
            .stdout
            .ends_with("Tag: entire message must remain visible\n")
    );
    assert_eq!(compact.summary()["text_presentation"]["process_width"], 32);
    assert_eq!(
        compact.summary()["text_presentation"]["process_names_truncated"],
        1
    );
    let full = s.text(&file, &["--process-width", "0"]);
    assert!(full.stdout.contains(name));
    assert_eq!(full.summary()["text_presentation"]["process_width"], 0);
    assert_eq!(
        full.summary()["text_presentation"]["process_names_truncated"],
        0
    );
    let jsonl = s.run(
        &["query", "--file", &file, "--all", "--format", "jsonl"],
        None,
    );
    assert_eq!(jsonl.summary()["records_returned"], 1);
    assert!(jsonl.summary().get("text_presentation").is_none());
    let row: Value = serde_json::from_str(&jsonl.stdout).unwrap();
    assert_eq!(row["process"]["name"], name);
    assert_eq!(row["message"], "entire message must remain visible\n");
    assert_eq!(fs::read(&file).unwrap(), before);
}

#[test]
fn process_width_is_display_columns_and_preserves_unicode_graphemes() {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let s = Sandbox::new();
    let name = "界界e\u{301}👩‍💻进程renderer末尾e\u{301}👩‍💻";
    let entry = proto_named("payload", 1, 0, Some(name));
    let file = s.native(vec![(entry, Some(1000))]);
    for width in [1usize, 2, 7, 12, 20] {
        let output = s.text(&file, &["--process-width", &width.to_string()]);
        let field = output
            .stdout
            .split_once(" I ")
            .unwrap()
            .1
            .split_once(" Tag: ")
            .unwrap()
            .0;
        assert!(UnicodeWidthStr::width(field) <= width, "{width}: {field:?}");
        assert!(field.contains('…'));
        let (prefix, suffix) = field.trim_end().split_once('…').unwrap();
        assert!(name.starts_with(prefix) && name.ends_with(suffix));
        let boundaries: Vec<_> = name
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain([name.len()])
            .collect();
        assert!(boundaries.contains(&prefix.len()));
        assert!(boundaries.contains(&(name.len() - suffix.len())));
    }
}
