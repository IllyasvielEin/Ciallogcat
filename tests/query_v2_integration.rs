//! Real CLI, isolated files, and deterministic byte-count assertions. No ADB.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use ciallogcat::document::{
    self, CaptureStatus, Document, DocumentHeader, ExportScope, NativeFormat,
};
use ciallogcat::log_store::LogStore;
use ciallogcat::model::{Level, LogBuffer, LogEntry};
use ciallogcat::record_io::{LegacyRecord, MAX_RECORD_BYTES};
use ciallogcat::session::Session;
use serde_json::{Value, json};

struct Sandbox {
    root: PathBuf,
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
impl Sandbox {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/query-v2-integration")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn path(&self, name: &str) -> String {
        self.root.join(name).to_str().unwrap().into()
    }
    fn run(&self, args: &[&str]) -> (bool, Value, usize) {
        let id = uuid::Uuid::new_v4();
        let out = self.root.join(format!("{id}.out"));
        let err = self.root.join(format!("{id}.err"));
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
                .args(args)
                .env("CIALLOGCAT_CONFIG_DIR", self.root.join("config"))
                .stdin(Stdio::null())
                .stdout(File::create(&out).unwrap())
                .stderr(File::create(&err).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "CLI timeout: {args:?}");
            std::thread::sleep(Duration::from_millis(5));
        };
        let bytes = fs::read(if status.success() { out } else { err }).unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("invalid JSON {e}: {}", String::from_utf8_lossy(&bytes)));
        (status.success(), value, bytes.len())
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (ok, value, _) = self.run(args);
        assert!(ok, "{args:?}: {value}");
        value
    }
    fn fail(&self, args: &[&str]) -> String {
        let (ok, value, _) = self.run(args);
        assert!(!ok, "unexpected success: {value}");
        value["error"]["code"].as_str().unwrap().into()
    }
    fn session(&self, rows: &[(u64, &str)]) -> String {
        let dir = self.root.join("session");
        fs::create_dir(&dir).unwrap();
        let mut bytes = Vec::new();
        for &(seq, message) in rows {
            bytes.extend(legacy(seq, message));
        }
        fs::write(dir.join("logs.jsonl"), &bytes).unwrap();
        self.status(rows.last().map_or(0, |r| r.0), bytes.len() as u64, None);
        dir.to_str().unwrap().into()
    }
    fn status(&self, seq: u64, bytes: u64, native: Option<&DocumentHeader>) {
        let mut status = json!({"schema_version":1,"type":"status","state":"running","entries":seq,"bytes":bytes,
            "updated_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64});
        if let Some(header) = native {
            status["record_schema_version"] = header.schema_version.into();
            status["artifact_id"] = header.artifact_id.to_string().into();
            status["session_id"] = header.session_id.unwrap().to_string().into();
            status["visible_end"] = json!({"seq":seq,"offset":bytes});
        }
        fs::write(
            self.root.join("session/status.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
    }
    fn native_status(&self, header: &DocumentHeader, seq: u64, bytes: usize, count: u64) {
        self.status(seq, bytes as u64, Some(header));
        let path = self.root.join("session/status.json");
        let mut status: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        status["entries"] = count.into();
        fs::write(path, serde_json::to_vec(&status).unwrap()).unwrap();
    }
}
fn legacy(seq: u64, message: &str) -> Vec<u8> {
    let entry = LogEntry::new("10-23 12:00:00.000", 42, 43, "Tag", Level::Info, message);
    let mut bytes = serde_json::to_vec(&LegacyRecord::from_entry(&entry, seq)).unwrap();
    bytes.push(b'\n');
    bytes
}
fn records(value: &Value) -> Vec<u64> {
    value["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect()
}
fn cursor(value: &Value) -> &str {
    value["next_cursor"].as_str().unwrap()
}
fn reference(value: &Value, index: usize) -> &str {
    value["records"][index]["record_ref"].as_str().unwrap()
}
fn native() -> (Document, Vec<u8>) {
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut store = LogStore::default();
    for mut entry in [
        LogEntry::new(
            "10-23 12:00:00.000",
            42,
            43,
            "Tag",
            Level::Error,
            "native 雪",
        ),
        LogEntry::marker("--------- beginning of main"),
    ] {
        entry.provenance = session.next_record(Some(1234));
        store.push(entry);
    }
    let doc = Document::from_snapshot(
        DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]),
        Arc::new(store),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap();
    let mut bytes = Vec::new();
    document::write_snapshot(&mut bytes, &doc, &AtomicBool::new(false)).unwrap();
    (doc, bytes)
}

fn native_with_footer_size(size: usize, newline: bool) -> (Document, Vec<u8>, usize) {
    native_with_footer_format(NativeFormat::V2, size, newline)
}

fn native_with_header_format(format: NativeFormat, size: usize) -> (DocumentHeader, Vec<u8>) {
    let (mut header, mut lines) = native_lines_format(format, &[1, 2]);
    header
        .source
        .extra
        .insert("future_diagnostics".into(), json!({"output":""}));
    let base = serde_json::to_vec(&header).unwrap().len() + 1;
    header.source.extra.get_mut("future_diagnostics").unwrap()["output"] =
        "x".repeat(size - base).into();
    lines[0] = serde_json::to_vec(&header).unwrap();
    lines[0].push(b'\n');
    assert_eq!(lines[0].len(), size);
    (header, lines.concat())
}

fn native_with_footer_format(
    format: NativeFormat,
    size: usize,
    newline: bool,
) -> (Document, Vec<u8>, usize) {
    let (mut doc, _) = native();
    doc.header.set_format(format).unwrap();
    doc.end.schema_version = doc.header.schema_version;
    let mut bytes = Vec::new();
    document::write_snapshot(&mut bytes, &doc, &AtomicBool::new(false)).unwrap();
    let start = bytes[..bytes.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap()
        + 1;
    doc.end.extra.insert("future_footer".into(), "".into());
    let base = serde_json::to_vec(&doc.end).unwrap().len() + usize::from(newline);
    doc.end
        .extra
        .insert("future_footer".into(), "x".repeat(size - base).into());
    bytes.truncate(start);
    serde_json::to_writer(&mut bytes, &doc.end).unwrap();
    if newline {
        bytes.push(b'\n');
    }
    assert_eq!(bytes.len() - start, size);
    (doc, bytes, start)
}

fn native_lines(sequences: &[u64]) -> (DocumentHeader, Vec<Vec<u8>>) {
    native_lines_format(NativeFormat::V2, sequences)
}

fn native_lines_format(format: NativeFormat, sequences: &[u64]) -> (DocumentHeader, Vec<Vec<u8>>) {
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut header = DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]);
    header.set_format(format).unwrap();
    let mut encoder = document::StreamEncoder::new(header.clone()).unwrap();
    let mut lines = vec![encoder.header_bytes().unwrap()];
    for &seq in sequences {
        let message = format!("row-{seq}");
        let mut entry = if format == NativeFormat::CompactV3 {
            let mut payload = b"\x08\x01\x10\x02\x18\x04\x28\x2a\x30\x2b\x42".to_vec();
            payload.push(message.len() as u8);
            payload.extend_from_slice(message.as_bytes());
            ciallogcat::protobuf::DecodedEntry::decode_payload(payload)
                .unwrap()
                .project()
        } else {
            LogEntry::new("now", 42, 43, "Tag", Level::Info, message)
        };
        let mut identity = session.next_record(Some(1234)).unwrap();
        identity.seq = seq;
        entry.provenance = Some(identity);
        lines.push(encoder.record_bytes(&entry).unwrap());
    }
    lines.push(encoder.finish_bytes(CaptureStatus::default()).unwrap());
    (header, lines)
}

fn cursor_payload(page: &Value) -> Value {
    // Inspect test tokens solely to assert protocol compatibility and counts.
    let hex = cursor(page).split('.').nth(1).unwrap();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn end_start_requires_a_native_tail_without_cursor_and_preserves_old_tokens() {
    let s = Sandbox::new();
    let dir = s.session(&[(1, "old")]);
    let file = s.path("session/logs.jsonl");
    for args in [
        vec!["query-v2", "--session-dir", &dir, "--start-at", "end"],
        vec![
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--start-at",
            "end",
        ],
        vec![
            "query-v2",
            "--file",
            &file,
            "--mode",
            "tail",
            "--start-at",
            "end",
        ],
        vec!["query-v2", "--session-dir", &dir, "--start-at", "unknown"],
    ] {
        assert_eq!(s.fail(&args), "invalid_arguments");
    }
    let beginning = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--start-at",
        "beginning",
    ]);
    assert_eq!(records(&beginning), [1]);
    assert_eq!(cursor_payload(&beginning)["version"], 2);
    assert!(cursor_payload(&beginning).get("tail_start").is_none());
    for start in ["beginning", "end"] {
        assert_eq!(
            s.fail(&[
                "query-v2",
                "--session-dir",
                &dir,
                "--mode",
                "tail",
                "--start-at",
                start,
                "--cursor",
                cursor(&beginning)
            ]),
            "invalid_arguments"
        );
    }
    let again = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&beginning),
    ]);
    assert_eq!(cursor(&again), cursor(&beginning));
}

#[test]
fn end_start_empty_native_session_sees_only_published_appends_and_footer() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let dir = s.session(&[]);
        let path = s.root.join("session/logs.jsonl");
        let (header, lines) = native_lines_format(format, &[1]);
        let args = [
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--start-at",
            "end",
        ];
        s.native_status(&header, 0, 0, 0);
        assert_eq!(s.fail(&args), "source_not_ready");
        fs::write(&path, &lines[0]).unwrap();
        s.native_status(&header, 0, lines[0].len(), 0);
        let start = s.ok(&args);
        assert!(records(&start).is_empty());
        assert_eq!(start["scan"]["records"], 0);
        assert_eq!(start["scan"]["bytes"], 0);
        assert_eq!(start["scan"]["next_offset"], lines[0].len());
        assert_eq!(start["validation"]["scope"], "full_prefix");
        assert_eq!(cursor_payload(&start)["version"], 3);
        fs::write(&path, lines.concat()).unwrap();
        let follow = [
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--cursor",
            cursor(&start),
        ];
        let unpublished = s.ok(&follow);
        assert!(records(&unpublished).is_empty());
        assert_eq!(cursor(&unpublished), cursor(&start));
        let record_end = lines[0].len() + lines[1].len();
        // Advancing either half of the published pair alone is rejected.
        s.native_status(&header, 1, lines[0].len(), 1);
        assert_eq!(s.fail(&follow), "invalid_source");
        s.native_status(&header, 0, record_end, 1);
        assert_eq!(s.fail(&follow), "invalid_source");
        s.native_status(&header, 1, record_end - 1, 1);
        assert_eq!(s.fail(&args), "invalid_source");
        s.native_status(&header, 1, record_end, 1);
        let row = s.ok(&follow);
        assert_eq!(records(&row), [1]);
        s.native_status(&header, 1, lines.concat().len(), 1);
        let finished = s.ok(&[
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--cursor",
            cursor(&row),
        ]);
        assert!(records(&finished).is_empty());
        assert_eq!(finished["artifact_finalized"], true);
        assert_eq!(finished["validation"]["footer_seen"], true);
        assert_eq!(cursor_payload(&finished)["count"], 1);

        // A truly empty stopped document also validates without fabricating seq=1.
        let (empty_header, empty_lines) = native_lines_format(format, &[]);
        fs::write(&path, empty_lines.concat()).unwrap();
        s.native_status(&empty_header, 0, empty_lines.concat().len(), 0);
        let empty = s.ok(&args);
        assert_eq!(empty["artifact_finalized"], true);
        assert_eq!(cursor_payload(&empty)["count"], 0);
        assert!(cursor_payload(&empty)["first_seq"].is_null());
    }
}

#[test]
fn end_start_sparse_sequences_keep_suffix_counts_and_filter_binding() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let dir = s.session(&[]);
        let path = s.root.join("session/logs.jsonl");
        let (header, lines) = native_lines_format(format, &[5, 9, 12, 20]);
        let prefix = lines[..3].concat();
        fs::write(&path, &prefix).unwrap();
        s.native_status(&header, 9, prefix.len(), 2);
        let start = s.ok(&[
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--start-at",
            "end",
            "--query",
            "row-20",
        ]);
        assert_eq!(cursor_payload(&start)["count"], 0);
        assert!(cursor_payload(&start)["first_seq"].is_null());
        assert_eq!(cursor_payload(&start)["tail_start"]["published_count"], 2);
        assert_eq!(cursor_payload(&start)["tail_start"]["first_seq"], 5);
        assert_eq!(start["validation"]["scope"], "tail_suffix");
        for mode in ["snapshot", "tail"] {
            assert_eq!(
                s.fail(&[
                    "query-v2",
                    "--session-dir",
                    &dir,
                    "--mode",
                    mode,
                    "--cursor",
                    cursor(&start),
                    "--query",
                    "changed"
                ]),
                "cursor_mismatch"
            );
        }
        fs::write(&path, lines.concat()).unwrap();
        s.native_status(&header, 20, lines.concat().len(), 4);
        let no_match = s.ok(&[
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--query",
            "row-20",
            "--cursor",
            cursor(&start),
            "--max-scan-records",
            "1",
        ]);
        assert!(records(&no_match).is_empty());
        assert_eq!(no_match["has_more"], true);
        assert_eq!(no_match["scan"]["last_seq"], 12);
        let finished = s.ok(&[
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--query",
            "row-20",
            "--cursor",
            cursor(&no_match),
        ]);
        assert_eq!(records(&finished), [20]);
        assert_eq!(finished["has_more"], false);
        assert_eq!(finished["artifact_finalized"], false);
        assert_eq!(finished["validation"]["footer_seen"], true);
        assert_eq!(cursor_payload(&finished)["count"], 2);
        assert_eq!(cursor_payload(&finished)["first_seq"], 12);
        let detail = s.ok(&[
            "get-v2",
            "--session-dir",
            &dir,
            "--ref",
            reference(&finished, 0),
        ]);
        assert_eq!(detail["seq"], 20);
    }
}

#[test]
fn end_start_large_history_has_bounded_io_without_claiming_prefix_validation() {
    let s = Sandbox::new();
    let dir = s.session(&[]);
    let path = s.root.join("session/logs.jsonl");
    let (header, mut lines) = native_lines(&(1..=30000).collect::<Vec<_>>());
    lines.pop(); // Activity with no footer yet.
    // An unscanned interior error must not turn the end cursor into a claim
    // that this history passed a full streaming validation.
    lines[20] = b"{invalid interior record}\n".to_vec();
    let bytes = lines.concat();
    assert!(bytes.len() > 8 * 1024 * 1024);
    fs::write(&path, &bytes).unwrap();
    s.native_status(&header, 30000, bytes.len(), 30000);
    let args = [
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--start-at",
        "end",
        "--max-scan-records",
        "1",
        "--max-scan-bytes",
        "8388609",
        "--max-response-bytes",
        "8192",
    ];
    let page = s.ok(&args);
    assert!(records(&page).is_empty());
    assert_eq!(page["scan"]["bytes"], 0);
    assert_eq!(page["scan"]["records"], 0);
    assert_eq!(page["scan"]["start_offset"], bytes.len());
    assert_eq!(page["scan"]["next_offset"], bytes.len());
    assert_eq!(page["snapshot"]["seq"], 30000);
    assert_eq!(page["has_more"], false);
    assert_eq!(page["validation"]["scope"], "tail_suffix");
    assert_eq!(page["validation"]["skipped_records"], 30000);
    assert!(
        page["scan"]["file_bytes_read"].as_u64().unwrap() < 32768,
        "{page}"
    );
    let again = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&page),
    ]);
    assert_eq!(cursor(&again), cursor(&page));
    assert!(again["scan"]["file_bytes_read"].as_u64().unwrap() < 16384);
    assert_eq!(
        s.fail(&["query-v2", "--session-dir", &dir, "--query", "no matches"]),
        "invalid_source"
    );
}

#[test]
fn end_start_validates_stopped_large_footers_and_retains_finalization_guard() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        for size in [
            70613,
            MAX_RECORD_BYTES as usize,
            MAX_RECORD_BYTES as usize + 1,
        ] {
            let s = Sandbox::new();
            let dir = s.session(&[]);
            let path = s.root.join("session/logs.jsonl");
            let (doc, bytes, footer_start) = native_with_footer_format(format, size, true);
            fs::write(&path, &bytes).unwrap();
            s.native_status(&doc.header, 2, bytes.len(), 2);
            let status_path = s.root.join("session/status.json");
            let mut status: Value =
                serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
            status["state"] = "completed".into();
            fs::write(status_path, serde_json::to_vec(&status).unwrap()).unwrap();
            let args = [
                "query-v2",
                "--session-dir",
                &dir,
                "--mode",
                "tail",
                "--start-at",
                "end",
            ];
            if size > MAX_RECORD_BYTES as usize {
                assert_eq!(s.fail(&args), "invalid_source");
                continue;
            }
            let page = s.ok(&args);
            assert!(records(&page).is_empty());
            assert_eq!(page["source_status"]["state"], "completed");
            assert_eq!(page["artifact_finalized"], false);
            assert_eq!(page["validation"]["footer_seen"], true);
            assert_eq!(page["scan"]["bytes"], 0);
            let read = page["scan"]["file_bytes_read"].as_u64().unwrap();
            assert!(
                read >= 2 * size as u64 && read < 2 * size as u64 + 65536,
                "{read}"
            );
            let follow = [
                "query-v2",
                "--session-dir",
                &dir,
                "--mode",
                "tail",
                "--cursor",
                cursor(&page),
            ];
            assert_eq!(cursor(&s.ok(&follow)), cursor(&page));
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"{}\n")
                .unwrap();
            s.native_status(&doc.header, 2, bytes.len() + 3, 2);
            assert_eq!(s.fail(&follow), "stale_cursor");
            // A stopped session may legitimately expose only its pre-footer prefix.
            s.native_status(&doc.header, 2, footer_start, 2);
            let prefix = s.ok(&args);
            assert_eq!(prefix["validation"]["footer_seen"], false);
        }
    }
}

#[test]
fn end_start_rejects_footer_errors_before_and_after_the_baseline() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        for field in [
            "record_count",
            "first_seq",
            "last_seq",
            "artifact_id",
            "finalized",
        ] {
            let s = Sandbox::new();
            let dir = s.session(&[]);
            let path = s.root.join("session/logs.jsonl");
            let (header, lines) = native_lines_format(format, &[5, 9, 12]);
            let prefix = lines[..3].concat();
            fs::write(&path, &prefix).unwrap();
            s.native_status(&header, 9, prefix.len(), 2);
            let args = [
                "query-v2",
                "--session-dir",
                &dir,
                "--mode",
                "tail",
                "--start-at",
                "end",
            ];
            let page = s.ok(&args);
            let mut footer: Value = serde_json::from_slice(lines.last().unwrap()).unwrap();
            match field {
                "record_count" => {
                    footer[field] = 2.into();
                    footer["scope"]["source_record_count"] = 2.into();
                }
                "first_seq" => {
                    footer[field] = 6.into();
                    footer["scope"]["source_first_seq"] = 6.into();
                }
                "last_seq" => {
                    footer[field] = 13.into();
                    footer["scope"]["source_last_seq"] = 13.into();
                }
                "artifact_id" => footer[field] = uuid::Uuid::new_v4().to_string().into(),
                _ => footer[field] = false.into(),
            }
            let mut bytes = lines[..4].concat();
            serde_json::to_writer(&mut bytes, &footer).unwrap();
            bytes.push(b'\n');
            fs::write(&path, &bytes).unwrap();
            s.native_status(&header, 12, bytes.len(), 3);
            assert_eq!(s.fail(&args), "invalid_source", "{field}");
            assert_eq!(
                s.fail(&[
                    "query-v2",
                    "--session-dir",
                    &dir,
                    "--mode",
                    "tail",
                    "--cursor",
                    cursor(&page)
                ]),
                "invalid_source",
                "{field}"
            );
        }
    }
}

#[test]
fn end_start_rejects_stale_source_identity_anchors_counts_and_boundaries() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        for mutation in [
            "replacement",
            "truncate",
            "anchor",
            "identity",
            "offset",
            "seq",
            "count",
        ] {
            let s = Sandbox::new();
            let dir = s.session(&[]);
            let path = s.root.join("session/logs.jsonl");
            let (header, lines) = native_lines_format(format, &[1, 2]);
            let bytes = lines[..3].concat();
            fs::write(&path, &bytes).unwrap();
            s.native_status(&header, 2, bytes.len(), 2);
            let page = s.ok(&[
                "query-v2",
                "--session-dir",
                &dir,
                "--mode",
                "tail",
                "--start-at",
                "end",
            ]);
            match mutation {
                "replacement" => {
                    fs::rename(&path, s.root.join("previous.jsonl")).unwrap();
                    fs::write(&path, &bytes).unwrap();
                }
                "truncate" => OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(bytes.len() as u64 - 1)
                    .unwrap(),
                "anchor" => {
                    let mut changed = bytes.clone();
                    changed[bytes.len() - 2] = b' ';
                    fs::write(&path, changed).unwrap();
                }
                "identity" => {
                    let (other, _) = native_lines_format(format, &[]);
                    s.native_status(&other, 2, bytes.len(), 2);
                }
                "offset" => s.native_status(&header, 1, lines[..2].concat().len(), 1),
                "seq" => s.native_status(&header, 1, bytes.len(), 2),
                "count" => s.native_status(&header, 2, bytes.len(), 1),
                _ => unreachable!(),
            }
            assert_eq!(
                s.fail(&[
                    "query-v2",
                    "--session-dir",
                    &dir,
                    "--mode",
                    "tail",
                    "--cursor",
                    cursor(&page)
                ]),
                if mutation == "count" {
                    "invalid_source"
                } else {
                    "stale_cursor"
                },
                "{mutation}"
            );
        }
    }
}

#[test]
fn snapshot_is_fixed_while_tail_advances_and_ignores_unpublished_bytes() {
    let s = Sandbox::new();
    let dir = s.session(&[(1, "one"), (2, "two"), (3, "three")]);
    let first = s.ok(&["query-v2", "--session-dir", &dir, "--limit", "1"]);
    let tail = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--limit",
        "1",
    ]);
    let path = s.root.join("session/logs.jsonl");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&legacy(4, "four"))
        .unwrap();
    let pending = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&tail),
    ]);
    assert_eq!(records(&pending), [2, 3]);
    s.status(4, fs::metadata(&path).unwrap().len(), None);
    let second = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--cursor",
        cursor(&first),
    ]);
    assert_eq!(records(&second), [2, 3]);
    assert_eq!(second["snapshot"]["seq"], 3);
    assert_eq!(second["has_more"], false);
    let fresh = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&pending),
    ]);
    assert_eq!(records(&fresh), [4]);
    let empty = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&fresh),
    ]);
    assert!(records(&empty).is_empty());
    assert_eq!(empty["scan"]["bytes"], 0);
    assert!(
        empty["scan"]["file_bytes_read"].as_u64().unwrap() < 2048,
        "{empty}"
    );
}

#[test]
fn deep_cursor_reads_delta_and_filtered_pages_make_progress() {
    let s = Sandbox::new();
    let dir = s.session(&[]);
    let path = s.root.join("session/logs.jsonl");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    for seq in 1..=6000 {
        file.write_all(&legacy(seq, &"payload ".repeat(32)))
            .unwrap();
    }
    drop(file);
    s.status(6000, fs::metadata(&path).unwrap().len(), None);
    let page = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--query",
        "not-present",
        "--max-scan-records",
        "5500",
    ]);
    assert_eq!(page["scan"]["last_seq"], 5500);
    assert_eq!(page["scan"]["stop_reason"], "scan_budget");
    let next = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--query",
        "not-present",
        "--cursor",
        cursor(&page),
        "--max-scan-records",
        "1",
    ]);
    assert_eq!(next["scan"]["last_seq"], 5501);
    assert!(
        next["scan"]["file_bytes_read"].as_u64().unwrap() < 32768,
        "{next}"
    );
    assert!(records(&next).is_empty());
    assert_eq!(next["has_more"], true);
}

#[test]
fn huge_first_record_returns_reference_and_full_record_is_recoverable_in_utf8_chunks() {
    let s = Sandbox::new();
    let message = "雪\"\\\n".repeat(120000);
    let file = s.path("large.jsonl");
    fs::write(&file, [legacy(1, &message), legacy(2, "next")].concat()).unwrap();
    let (ok, page, size) = s.run(&[
        "query-v2",
        "--file",
        &file,
        "--max-response-bytes",
        "8192",
        "--preview-bytes",
        "16384",
        "--limit",
        "1",
    ]);
    assert!(ok, "{page}");
    assert!(size <= 8192);
    assert_eq!(records(&page), [1]);
    assert_eq!(page["records"][0]["preview_truncated"], true);
    assert!(page["scan"]["next_offset"].as_u64().unwrap() > 0);
    let next = s.ok(&["query-v2", "--file", &file, "--cursor", cursor(&page)]);
    assert_eq!(records(&next), [2]);
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let (ok, part, size) = s.run(&[
            "get-v2",
            "--file",
            &file,
            "--ref",
            reference(&page, 0),
            "--text-offset",
            &offset.to_string(),
            "--max-response-bytes",
            "1048576",
        ]);
        assert!(ok, "{part}");
        assert!(size <= 1048576);
        text.push_str(part["record_json_chunk"].as_str().unwrap());
        if part["complete"] == true {
            break;
        }
        let next = part["next_text_offset"].as_u64().unwrap();
        assert!(next > offset);
        offset = next;
    }
    let record: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(record["message"], message);
    let bad = text.find('雪').unwrap() + 1;
    assert_eq!(
        s.fail(&[
            "get-v2",
            "--file",
            &file,
            "--ref",
            reference(&page, 0),
            "--text-offset",
            &bad.to_string()
        ]),
        "invalid_arguments"
    );
}

#[test]
fn tampering_changed_conditions_cross_source_replacement_and_truncation_are_rejected() {
    let s = Sandbox::new();
    let file = s.path("logs.jsonl");
    fs::write(&file, [legacy(1, "one"), legacy(2, "two")].concat()).unwrap();
    let page = s.ok(&["query-v2", "--file", &file, "--limit", "1"]);
    let token = cursor(&page);
    let mut bad = token.as_bytes().to_vec();
    let last = bad.len() - 1;
    bad[last] = if bad[last] == b'0' { b'1' } else { b'0' };
    assert_eq!(
        s.fail(&[
            "query-v2",
            "--file",
            &file,
            "--cursor",
            std::str::from_utf8(&bad).unwrap()
        ]),
        "invalid_token"
    );
    assert_eq!(
        s.fail(&[
            "query-v2", "--file", &file, "--cursor", token, "--query", "changed"
        ]),
        "cursor_mismatch"
    );
    let other = s.path("other.jsonl");
    fs::copy(&file, &other).unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--file", &other, "--cursor", token]),
        "stale_cursor"
    );
    fs::rename(&file, s.path("old.jsonl")).unwrap();
    fs::copy(&other, &file).unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--file", &file, "--cursor", token]),
        "stale_cursor"
    );
    let new = s.ok(&["query-v2", "--file", &file, "--limit", "1"]);
    OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(10)
        .unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--file", &file, "--cursor", cursor(&new)]),
        "stale_cursor"
    );
}

#[test]
fn nearby_context_uses_bounded_backward_reads_and_preserves_target() {
    let s = Sandbox::new();
    let file = s.path("context.jsonl");
    let mut writer = File::create(&file).unwrap();
    for seq in 1..=6000 {
        writer
            .write_all(&legacy(seq, if seq == 5500 { "target" } else { "nearby" }))
            .unwrap();
    }
    drop(writer);
    let page = s.ok(&["query-v2", "--file", &file, "--query", "target"]);
    assert_eq!(records(&page), [5500]);
    let context = s.ok(&[
        "context-v2",
        "--file",
        &file,
        "--ref",
        reference(&page, 0),
        "--before",
        "2",
        "--after",
        "2",
    ]);
    assert_eq!(records(&context), [5498, 5499, 5500, 5501, 5502]);
    assert_eq!(context["partial_before"], false);
    assert_eq!(context["partial_after"], false);
    assert!(
        context["scan"]["file_bytes_read"].as_u64().unwrap() < 65536,
        "{context}"
    );
    let clipped = s.ok(&[
        "context-v2",
        "--file",
        &file,
        "--ref",
        reference(&page, 0),
        "--before",
        "100",
        "--after",
        "100",
        "--max-response-bytes",
        "8192",
    ]);
    assert!(records(&clipped).contains(&5500));
    assert!(clipped["partial_before"] == true || clipped["partial_after"] == true);
}

#[test]
fn native_offline_and_active_stream_share_codec_and_stable_identities() {
    let s = Sandbox::new();
    let (doc, bytes) = native();
    let file = s.path("native.jsonl");
    fs::write(&file, &bytes).unwrap();
    let page = s.ok(&["query-v2", "--file", &file]);
    assert_eq!(records(&page), [1, 2]);
    assert_eq!(page["artifact_finalized"], true);
    assert_eq!(page["records"][1]["kind"], "event");
    assert_eq!(
        page["records"][0]["identity"]["session_id"],
        doc.header.session_id.unwrap().to_string()
    );
    let detail = s.ok(&["get-v2", "--file", &file, "--ref", reference(&page, 0)]);
    let original: Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(1).unwrap()).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(detail["record_json_chunk"].as_str().unwrap()).unwrap(),
        original
    );
    let parts: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
    let partial = [parts[0], parts[1]].concat();
    let incomplete = s.path("incomplete.jsonl");
    fs::write(&incomplete, &partial).unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--file", &incomplete]),
        "invalid_source"
    );
    let dir = s.session(&[]);
    fs::write(s.root.join("session/logs.jsonl"), &partial).unwrap();
    s.status(1, partial.len() as u64, Some(&doc.header));
    let active = s.ok(&["query-v2", "--session-dir", &dir, "--mode", "tail"]);
    assert_eq!(records(&active), [1]);
    assert_eq!(active["artifact_finalized"], false);
    OpenOptions::new()
        .append(true)
        .open(s.root.join("session/logs.jsonl"))
        .unwrap()
        .write_all(&[parts[2], parts[3]].concat())
        .unwrap();
    s.status(2, bytes.len() as u64, Some(&doc.header));
    let ended = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&active),
    ]);
    assert_eq!(records(&ended), [2]);
    assert_eq!(ended["artifact_finalized"], true);
}

#[test]
fn native_large_header_supports_file_and_session_paging_get_and_context() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let header_bytes = 72 * 1024;
        let (header, mut bytes) = native_with_header_format(format, header_bytes);
        let loaded = document::load(&bytes[..], 64 * 1024 * 1024, &AtomicBool::new(false)).unwrap();
        assert_eq!(loaded.header.source.extra, header.source.extra);
        let dir = s.session(&[]);
        let file = s.path("session/logs.jsonl");
        fs::write(&file, &bytes).unwrap();
        s.status(2, bytes.len() as u64, Some(&header));
        let mut pages = Vec::new();
        for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
            let first = s.ok(&["query-v2", selector, path, "--limit", "1"]);
            assert_eq!(records(&first), [1]);
            assert_eq!(first["source_schema_version"], header.schema_version);
            assert_eq!(first["scan"]["start_offset"], header_bytes);
            assert_eq!(first["has_more"], true);
            let ended = s.ok(&["query-v2", selector, path, "--cursor", cursor(&first)]);
            assert_eq!(records(&ended), [2]);
            assert_eq!(ended["artifact_finalized"], true);
            assert_eq!(ended["snapshot"]["offset"], bytes.len());
            let empty = s.ok(&["query-v2", selector, path, "--cursor", cursor(&ended)]);
            assert!(records(&empty).is_empty());
            assert_eq!(empty["scan"]["bytes"], 0);
            for page in [&first, &ended, &empty] {
                // Opening rereads bounded header metadata outside the logical
                // scan budget, including on cursor continuations.
                let read = page["scan"]["file_bytes_read"].as_u64().unwrap();
                assert!(read >= header_bytes as u64, "{page}");
                assert!(read <= header_bytes as u64 + 32768, "{page}");
            }
            let detail = s.ok(&["get-v2", selector, path, "--ref", reference(&first, 0)]);
            let record: Value =
                serde_json::from_str(detail["record_json_chunk"].as_str().unwrap()).unwrap();
            assert_eq!(record["seq"], 1);
            let read = detail["file_bytes_read"].as_u64().unwrap();
            assert!(read >= header_bytes as u64 && read <= header_bytes as u64 + 32768);
            let context = s.ok(&["context-v2", selector, path, "--ref", reference(&first, 0)]);
            assert_eq!(records(&context), [1, 2]);
            let read = context["scan"]["file_bytes_read"].as_u64().unwrap();
            assert!(read >= header_bytes as u64 && read <= header_bytes as u64 + 32768);
            let other = if selector == "--file" {
                ("--session-dir", &dir)
            } else {
                ("--file", &file)
            };
            assert_eq!(
                s.fail(&["query-v2", other.0, other.1, "--cursor", cursor(&ended)]),
                "stale_cursor"
            );
            pages.push(first);
        }
        // Preserve inode, length, schema and artifact identity while modifying
        // valid header content within the existing fixed identity prefix.
        let changed = bytes.windows(4).position(|part| part == b"TEST").unwrap();
        assert!(changed < 512);
        bytes[changed] = b'B';
        document::decode_header(&bytes[..header_bytes]).unwrap();
        fs::write(&file, &bytes).unwrap();
        for ((selector, path), page) in [("--file", &file), ("--session-dir", &dir)]
            .into_iter()
            .zip(&pages)
        {
            assert_eq!(
                s.fail(&["query-v2", selector, path, "--cursor", cursor(page)]),
                "stale_cursor"
            );
            assert_eq!(
                s.fail(&["get-v2", selector, path, "--ref", reference(page, 0)]),
                "stale_cursor"
            );
        }
    }
}

#[test]
fn native_header_encoded_limit_and_truncation_follow_the_shared_codec() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let dir = s.session(&[]);
        let file = s.path("session/logs.jsonl");
        let cap = MAX_RECORD_BYTES as usize;
        let (header, bytes) = native_with_header_format(format, cap);
        document::decode_header(&bytes[..cap]).unwrap();
        fs::write(&file, &bytes).unwrap();
        s.status(2, bytes.len() as u64, Some(&header));
        let scan_budget = (MAX_RECORD_BYTES + 1).to_string();
        for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
            let first = s.ok(&["query-v2", selector, path, "--max-scan-bytes", &scan_budget]);
            assert_eq!(records(&first), [1]);
            assert_eq!(first["scan"]["start_offset"], cap);
            assert_eq!(first["scan"]["stop_reason"], "scan_budget");
            assert!(first["scan"]["bytes"].as_u64().unwrap() < 8192);
            // A maximal header is control I/O outside the logical scan budget.
            let read = first["scan"]["file_bytes_read"].as_u64().unwrap();
            assert!(read > MAX_RECORD_BYTES + 1 && read <= MAX_RECORD_BYTES + 32768);
            let ended = s.ok(&["query-v2", selector, path, "--cursor", cursor(&first)]);
            assert_eq!(records(&ended), [2]);
            assert_eq!(ended["artifact_finalized"], true);
            let read = ended["scan"]["file_bytes_read"].as_u64().unwrap();
            assert!((MAX_RECORD_BYTES..=MAX_RECORD_BYTES + 32768).contains(&read));
        }
        let (header, bytes) = native_with_header_format(format, cap + 1);
        // The JSON itself fits; the LF makes this encoded line one byte too big.
        document::decode_header(&bytes[..cap]).unwrap();
        assert!(document::decode_header(&bytes[..cap + 1]).is_err());
        fs::write(&file, &bytes).unwrap();
        s.status(2, bytes.len() as u64, Some(&header));
        for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
            assert_eq!(s.fail(&["query-v2", selector, path]), "invalid_source");
        }
        let header_bytes = 72 * 1024;
        let (header, bytes) = native_with_header_format(format, header_bytes);
        for cut in [header_bytes - 128, header_bytes - 1] {
            fs::write(&file, &bytes[..cut]).unwrap();
            s.status(0, cut as u64, Some(&header));
            for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
                assert_eq!(s.fail(&["query-v2", selector, path]), "invalid_source");
            }
        }
    }
}

#[test]
fn native_large_header_tail_obeys_published_watermarks() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let header_bytes = 72 * 1024;
        let (header, bytes) = native_with_header_format(format, header_bytes);
        let dir = s.session(&[]);
        fs::write(s.root.join("session/logs.jsonl"), &bytes).unwrap();
        let args = [
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--start-at",
            "end",
        ];
        s.status(0, 0, Some(&header));
        assert_eq!(s.fail(&args), "source_not_ready");
        for cut in [70 * 1024, header_bytes - 1] {
            s.status(0, cut as u64, Some(&header));
            assert_eq!(s.fail(&args), "invalid_source");
        }
        s.status(0, header_bytes as u64, Some(&header));
        let start = s.ok(&args);
        assert!(records(&start).is_empty());
        assert_eq!(start["snapshot"]["offset"], header_bytes);
        assert_eq!(start["scan"]["bytes"], 0);
        assert_eq!(start["artifact_finalized"], false);
        let read = start["scan"]["file_bytes_read"].as_u64().unwrap();
        assert!(read >= header_bytes as u64 && read <= header_bytes as u64 + 32768);
        let first_end = header_bytes
            + bytes[header_bytes..]
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap()
            + 1;
        let follow = [
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--cursor",
            cursor(&start),
        ];
        s.status(1, first_end as u64 - 1, Some(&header));
        assert_eq!(s.fail(&follow), "invalid_source");
        s.status(1, first_end as u64, Some(&header));
        let first = s.ok(&follow);
        assert_eq!(records(&first), [1]);
        assert_eq!(first["snapshot"]["offset"], first_end);
        let follow = [
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--cursor",
            cursor(&first),
        ];
        let empty = s.ok(&follow);
        assert!(records(&empty).is_empty());
        assert_eq!(cursor(&empty), cursor(&first));
        s.status(2, bytes.len() as u64, Some(&header));
        let ended = s.ok(&follow);
        assert_eq!(records(&ended), [2]);
        assert_eq!(ended["artifact_finalized"], true);
        assert_eq!(ended["validation"]["footer_seen"], true);
    }
}

#[test]
fn native_large_footer_supports_file_and_session_paging_get_and_context() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let (doc, bytes, footer_start) = native_with_footer_format(format, 70_613, true);
        let footer_bytes = (bytes.len() - footer_start) as u64;
        let dir = s.session(&[]);
        let file = s.path("session/logs.jsonl");
        fs::write(&file, &bytes).unwrap();
        s.status(2, bytes.len() as u64, Some(&doc.header));
        let scan_budget = (MAX_RECORD_BYTES + 1).to_string();
        for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
            let first = s.ok(&["query-v2", selector, path, "--max-scan-records", "2"]);
            assert_eq!(records(&first), [1, 2]);
            assert_eq!(first["artifact_finalized"], false);
            assert_eq!(first["has_more"], true);
            assert_eq!(first["scan"]["stop_reason"], "scan_budget");
            assert_eq!(first["scan"]["next_offset"], footer_start);
            let (ok, ended, response_bytes) = s.run(&[
                "query-v2",
                selector,
                path,
                "--cursor",
                cursor(&first),
                "--max-scan-bytes",
                &scan_budget,
                "--max-response-bytes",
                "8192",
            ]);
            assert!(ok, "{ended}");
            assert!(response_bytes <= 8192);
            assert!(records(&ended).is_empty());
            assert_eq!(ended["artifact_finalized"], true);
            assert_eq!(ended["has_more"], false);
            assert_eq!(ended["scan"]["bytes"], footer_bytes);
            assert_eq!(ended["scan"]["records"], 0);
            assert_eq!(ended["snapshot"]["seq"], 2);
            // Offline opening locates and validates the footer before the scan;
            // all three bounded reads must remain included in the I/O accounting.
            let read = ended["scan"]["file_bytes_read"].as_u64().unwrap();
            assert!(
                read >= footer_bytes && read <= 3 * footer_bytes + 32768,
                "{ended}"
            );
            if selector == "--file" {
                assert!(read >= 3 * footer_bytes);
                assert_eq!(
                    ended["source_status"]["capture_status_source"],
                    "artifact_footer"
                );
            }
            let empty = s.ok(&["query-v2", selector, path, "--cursor", cursor(&ended)]);
            assert!(records(&empty).is_empty());
            assert_eq!(empty["scan"]["bytes"], 0);
            assert_eq!(empty["artifact_finalized"], true);
            assert!(empty["scan"]["file_bytes_read"].as_u64().unwrap() <= 2 * footer_bytes + 32768);
            let detail = s.ok(&["get-v2", selector, path, "--ref", reference(&first, 0)]);
            let record: Value =
                serde_json::from_str(detail["record_json_chunk"].as_str().unwrap()).unwrap();
            assert_eq!(record["seq"], 1);
            let context = s.ok(&["context-v2", selector, path, "--ref", reference(&first, 0)]);
            assert_eq!(records(&context), [1, 2]);
            assert_eq!(context["partial_after"], false);
            assert!(
                context["scan"]["file_bytes_read"].as_u64().unwrap() <= 3 * footer_bytes + 32768
            );
            let other = if selector == "--file" {
                ("--session-dir", &dir)
            } else {
                ("--file", &file)
            };
            assert_eq!(
                s.fail(&["query-v2", other.0, other.1, "--cursor", cursor(&ended)]),
                "stale_cursor"
            );
        }
    }
}

#[test]
fn native_footer_encoded_limit_and_truncation_follow_the_shared_codec() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let dir = s.session(&[]);
        let file = s.path("session/logs.jsonl");
        for newline in [true, false] {
            // Include data before the footer: locating an exactly maximal footer
            // requires looking one byte farther back for its preceding separator.
            let (doc, bytes, footer_start) =
                native_with_footer_format(format, MAX_RECORD_BYTES as usize, newline);
            document::decode_end(&bytes[footer_start..], &doc.header).unwrap();
            fs::write(&file, &bytes).unwrap();
            s.status(2, bytes.len() as u64, Some(&doc.header));
            let offline = s.ok(&["query-v2", "--file", &file]);
            assert_eq!(records(&offline), [1, 2]);
            assert_eq!(offline["artifact_finalized"], true);
            assert!(
                offline["scan"]["file_bytes_read"].as_u64().unwrap()
                    <= 3 * MAX_RECORD_BYTES + 32768
            );
            if newline {
                let active = s.ok(&["query-v2", "--session-dir", &dir]);
                assert_eq!(records(&active), [1, 2]);
                assert_eq!(active["artifact_finalized"], true);
                assert!(
                    active["scan"]["file_bytes_read"].as_u64().unwrap() <= MAX_RECORD_BYTES + 32768
                );
            } else {
                assert_eq!(
                    s.fail(&["query-v2", "--session-dir", &dir]),
                    "invalid_source"
                );
            }
            let (doc, bytes, footer_start) =
                native_with_footer_format(format, MAX_RECORD_BYTES as usize + 1, newline);
            assert!(document::decode_end(&bytes[footer_start..], &doc.header).is_err());
            fs::write(&file, &bytes).unwrap();
            s.status(2, bytes.len() as u64, Some(&doc.header));
            for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
                assert_eq!(s.fail(&["query-v2", selector, path]), "invalid_source");
            }
        }
        let (doc, mut bytes, _) = native_with_footer_format(format, 70_613, true);
        bytes.truncate(bytes.len() - 128);
        fs::write(&file, &bytes).unwrap();
        s.status(2, bytes.len() as u64, Some(&doc.header));
        for (selector, path) in [("--file", &file), ("--session-dir", &dir)] {
            assert_eq!(s.fail(&["query-v2", selector, path]), "invalid_source");
        }
    }
}

#[test]
fn native_large_footer_tail_obeys_watermarks_and_cursor_anchors() {
    let s = Sandbox::new();
    let (doc, mut bytes, footer_start) = native_with_footer_size(70_613, true);
    let dir = s.session(&[]);
    let file = s.path("session/logs.jsonl");
    fs::write(&file, &bytes).unwrap();
    s.status(2, footer_start as u64, Some(&doc.header));
    let first = s.ok(&["query-v2", "--session-dir", &dir, "--mode", "tail"]);
    assert_eq!(records(&first), [1, 2]);
    assert_eq!(first["artifact_finalized"], false);
    assert!(first["scan"]["file_bytes_read"].as_u64().unwrap() < 32768);
    let args = [
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--cursor",
        cursor(&first),
    ];
    let empty = s.ok(&args);
    assert!(records(&empty).is_empty());
    assert_eq!(empty["scan"]["bytes"], 0);
    assert!(empty["scan"]["file_bytes_read"].as_u64().unwrap() < 32768);
    s.status(2, (footer_start + 100) as u64, Some(&doc.header));
    assert_eq!(s.fail(&args), "invalid_source");
    s.status(2, bytes.len() as u64, Some(&doc.header));
    let ended = s.ok(&args);
    assert!(records(&ended).is_empty());
    assert_eq!(ended["artifact_finalized"], true);
    assert_eq!(ended["scan"]["bytes"], bytes.len() - footer_start);
    let offline = s.ok(&["query-v2", "--file", &file]);
    // Change valid extension data inside the observed final boundary, keeping
    // file identity/length and the small prefix intact.
    let changed = bytes.len() - 16;
    assert_eq!(bytes[changed], b'x');
    bytes[changed] = b'y';
    document::decode_end(&bytes[footer_start..], &doc.header).unwrap();
    fs::write(&file, &bytes).unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--file", &file, "--cursor", cursor(&offline)]),
        "stale_cursor"
    );
    assert_eq!(
        s.fail(&[
            "query-v2",
            "--session-dir",
            &dir,
            "--mode",
            "tail",
            "--cursor",
            cursor(&ended)
        ]),
        "stale_cursor"
    );
}

#[test]
fn active_partial_watermark_and_inconsistent_native_footer_are_errors() {
    let s = Sandbox::new();
    let dir = s.session(&[(1, "one")]);
    let path = s.root.join("session/logs.jsonl");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"schema_version\":1")
        .unwrap();
    s.status(2, fs::metadata(&path).unwrap().len(), None);
    assert_eq!(
        s.fail(&["query-v2", "--session-dir", &dir]),
        "invalid_source"
    );
    let (_, bytes) = native();
    let mut lines: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    lines[1]["seq"] = 7.into();
    lines[1]["provenance"]["seq"] = 7.into();
    let file = s.path("bad-native.jsonl");
    let mut output = File::create(&file).unwrap();
    for line in lines {
        serde_json::to_writer(&mut output, &line).unwrap();
        output.write_all(b"\n").unwrap();
    }
    drop(output);
    assert_eq!(s.fail(&["query-v2", "--file", &file]), "invalid_source");
}

#[test]
fn empty_files_and_utf8_preview_have_explicit_small_results() {
    let s = Sandbox::new();
    let file = s.path("empty.jsonl");
    fs::write(&file, []).unwrap();
    let empty = s.ok(&["query-v2", "--file", &file]);
    assert!(records(&empty).is_empty());
    assert_eq!(empty["has_more"], false);
    fs::write(
        &file,
        legacy(1, "雪雪literal [truncated: log line exceeded 1 MiB]"),
    )
    .unwrap();
    let page = s.ok(&["query-v2", "--file", &file, "--preview-bytes", "4"]);
    assert_eq!(page["records"][0]["message_preview"], "雪");
    assert_eq!(page["records"][0]["identity_scope"], "artifact_local");
    assert!(page["records"][0]["identity"].is_null());
    assert!(page["records"][0]["text_loss"].is_null());
    assert_eq!(
        s.fail(&["query-v2", "--file", &file, "--max-scan-bytes", "1024"]),
        "invalid_arguments"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(s.root.join("config/query-v2.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn metadata_and_key_validation_are_bounded_and_diagnostic_previews_fit_the_response() {
    let s = Sandbox::new();
    let dir = s.session(&[(1, "target")]);
    let path = s.root.join("session/status.json");
    let mut status: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    status["type"] = "unexpected".into();
    fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--session-dir", &dir]),
        "invalid_source"
    );
    status["type"] = "status".into();
    status["state"] = "failed".into();
    status["error"] = "\0雪".repeat(1000).into();
    status["last_diagnostic"] = "\n\t".repeat(1000).into();
    fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();
    let page = s.ok(&["query-v2", "--session-dir", &dir]);
    assert_eq!(page["source_status"]["diagnostic_preview_truncated"], true);
    assert!(page["source_status"]["error"].as_str().unwrap().len() <= 96);
    let (ok, context, size) = s.run(&[
        "context-v2",
        "--session-dir",
        &dir,
        "--ref",
        reference(&page, 0),
        "--max-response-bytes",
        "8192",
    ]);
    assert!(ok, "{context}");
    assert!(size <= 8192);
    // A sparse, incorrect key is rejected from metadata before any large read.
    OpenOptions::new()
        .write(true)
        .open(s.root.join("config/query-v2.key"))
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--session-dir", &dir]),
        "token_key_unavailable"
    );
    OpenOptions::new()
        .write(true)
        .open(s.root.join("config/query-v2.key"))
        .unwrap()
        .set_len(32)
        .unwrap();
}

#[test]
fn response_and_scan_budgets_preserve_progress_through_escaped_records() {
    let s = Sandbox::new();
    let file = s.path("escaped.jsonl");
    let mut bytes = Vec::new();
    for seq in 1..=80 {
        bytes.extend(legacy(seq, &format!("{seq} {}", "\0\n\"\\雪".repeat(400))));
    }
    fs::write(&file, &bytes).unwrap();
    let mut next: Option<String> = None;
    let mut seen = Vec::new();
    for _ in 0..81 {
        let mut args = vec![
            "query-v2",
            "--file",
            &file,
            "--max-response-bytes",
            "8192",
            "--preview-bytes",
            "16384",
            "--max-scan-records",
            "3",
        ];
        if let Some(token) = &next {
            args.extend(["--cursor", token]);
        }
        let (ok, page, length) = s.run(&args);
        assert!(ok, "{page}");
        assert!(length <= 8192);
        assert!(page["scan"]["records"].as_u64().unwrap() <= 3);
        let current = records(&page);
        assert!(!current.is_empty(), "first matching record must advance");
        seen.extend(current);
        if page["has_more"] == false {
            break;
        }
        next = Some(cursor(&page).to_owned());
    }
    assert_eq!(seen, (1..=80).collect::<Vec<_>>());
}

#[test]
fn active_append_during_snapshot_query_is_ignored_until_published_then_tail_catches_up() {
    let s = Sandbox::new();
    let dir = s.session(&[]);
    let path = s.root.join("session/logs.jsonl");
    let mut initial = Vec::new();
    for seq in 1..=1000 {
        initial.extend(legacy(seq, "initial"));
    }
    fs::write(&path, &initial).unwrap();
    s.status(1000, initial.len() as u64, None);
    // Writer and query actually overlap. The unchanged status is a deterministic
    // boundary, so scheduling cannot alter which records the first query sees.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&legacy(1001, "append")).unwrap();
        ready_tx.send(()).unwrap();
        let mut seq = 1001;
        while seq < 2000 && finish_rx.try_recv().is_err() {
            seq += 1;
            file.write_all(&legacy(seq, "append")).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        seq
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let fixed = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--query",
        "absent",
    ]);
    let _ = finish_tx.send(());
    let last = writer.join().unwrap();
    assert_eq!(fixed["scan"]["last_seq"], 1000);
    assert_eq!(fixed["snapshot"]["offset"], initial.len() as u64);
    s.status(
        last,
        fs::metadata(s.root.join("session/logs.jsonl"))
            .unwrap()
            .len(),
        None,
    );
    let appended = s.ok(&[
        "query-v2",
        "--session-dir",
        &dir,
        "--mode",
        "tail",
        "--query",
        "absent",
        "--cursor",
        cursor(&fixed),
    ]);
    assert_eq!(appended["scan"]["last_seq"], last);
    assert_eq!(appended["scan"]["records"], last - 1000);
}

fn proto_document(message: &[u8], format: NativeFormat) -> (Document, Vec<u8>) {
    // Include an unknown wire field and a duplicate known field: raw evidence
    // must survive, rather than being regenerated from the effective values.
    let mut payload =
        b"\x08\x01\x10\x02\x18\x04\x28\x04\x30\x05\x3a\x04Tag\0\x4a\x03app\x98\x06\x01\x42\x00\x42"
            .to_vec();
    let mut length = message.len();
    while length >= 128 {
        payload.push((length as u8 & 127) | 128);
        length >>= 7;
    }
    payload.push(length as u8);
    payload.extend_from_slice(message);
    let mut entry = ciallogcat::protobuf::DecodedEntry::decode_payload(payload)
        .unwrap()
        .project();
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    entry.provenance = session.next_record(Some(123));
    let mut store = LogStore::default();
    store.push(entry);
    let mut marker = LogEntry::marker("marker");
    marker.provenance = session.next_record(Some(124));
    store.push(marker);
    let mut header = DocumentHeader::live(session.id(), "TEST".into(), vec![LogBuffer::Main]);
    header.source.transport_format = Some("logcat_proto_experimental".into());
    header.set_format(format).unwrap();
    let mut doc = Document::from_snapshot(
        header,
        Arc::new(store),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap();
    let meta = &mut doc.record_metadata[0];
    for extra in [
        &mut meta.extra,
        &mut meta.proto_extra,
        &mut meta.provenance_extra,
        &mut meta.text_loss_extra,
        &mut meta.device_time_extra,
        &mut meta.process_extra,
    ] {
        extra.insert("future".into(), json!({"nested":["雪", 42, null]}));
    }
    if format == NativeFormat::CompactV3 {
        meta.projection_extra
            .insert("future_projection".into(), json!([1, "雪"]));
    }
    let mut bytes = Vec::new();
    document::write_snapshot(&mut bytes, &doc, &AtomicBool::new(false)).unwrap();
    (doc, bytes)
}

fn get_chunks(s: &Sandbox, file: &str, record_ref: &str, view: &str, budget: usize) -> String {
    let mut result = String::new();
    let mut offset = 0;
    loop {
        let (ok, response, size) = s.run(&[
            "get-v2",
            "--file",
            file,
            "--ref",
            record_ref,
            "--view",
            view,
            "--text-offset",
            &offset.to_string(),
            "--max-response-bytes",
            &budget.to_string(),
        ]);
        assert!(ok, "{response}");
        assert!(size <= budget);
        assert_eq!(response["view"], view);
        assert_eq!(response["derived"], view == "decoded");
        assert_eq!(response["text_offset"], offset);
        result.push_str(response["record_json_chunk"].as_str().unwrap());
        if response["complete"] == true {
            assert!(response["next_text_offset"].is_null());
            assert_eq!(response["record_bytes"], result.len());
            return result;
        }
        let next = response["next_text_offset"].as_u64().unwrap() as usize;
        assert!(next > offset);
        assert_eq!(next, result.len());
        offset = next;
    }
}

#[test]
fn native_proto_stored_and_decoded_views_preserve_full_fields_extensions_and_utf8_chunks() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let s = Sandbox::new();
        let message = "雪\0\n\\\" ".repeat(4000);
        let (doc, bytes) = proto_document(message.as_bytes(), format);
        let file = s.path("proto.jsonl");
        fs::write(&file, &bytes).unwrap();
        let page = s.ok(&[
            "query-v2",
            "--file",
            &file,
            "--query",
            "雪",
            "--preview-bytes",
            "3",
        ]);
        assert_eq!(page["schema_version"], 2);
        assert_eq!(page["source_schema_version"], doc.header.schema_version);
        assert_eq!(page["records"][0]["message_preview"], "雪");
        assert_eq!(page["records"][0]["preview_truncated"], true);
        let record_ref = reference(&page, 0);
        let stored = get_chunks(&s, &file, record_ref, "stored", 8192);
        let raw_line = bytes.split(|b| *b == b'\n').nth(1).unwrap();
        assert_eq!(stored.as_bytes(), raw_line);
        let default = s.ok(&["get-v2", "--file", &file, "--ref", record_ref]);
        assert_eq!(default["view"], "stored");
        assert_eq!(default["derived"], false);
        assert!(stored.starts_with(default["record_json_chunk"].as_str().unwrap()));
        let decoded_text = get_chunks(&s, &file, record_ref, "decoded", 8192);
        let decoded: Value = serde_json::from_str(&decoded_text).unwrap();
        assert_eq!(decoded["message"], message);
        assert_eq!(decoded["device_time"]["text"], "1.000000002");
        assert_eq!(decoded["pid"], 4);
        assert_eq!(decoded["tid"], 5);
        assert_eq!(decoded["level"], "I");
        assert_eq!(decoded["process"]["name"], "app");
        assert_eq!(decoded["process"]["source"], "device_process_cache");
        assert_eq!(decoded["tag"], "Tag");
        let expected_extra = json!({"nested":["雪",42,null]});
        assert_eq!(decoded["future"], expected_extra);
        for parent in ["proto", "provenance", "text_loss", "device_time", "process"] {
            assert_eq!(decoded[parent]["future"], expected_extra, "{parent}");
        }
        use base64::Engine;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(decoded["proto"]["raw_payload"].as_str().unwrap())
                .unwrap(),
            doc.entries[0].proto.as_ref().unwrap().raw_payload
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(decoded["proto"]["message_bytes"].as_str().unwrap())
                .unwrap(),
            message.as_bytes()
        );
        if format == NativeFormat::CompactV3 {
            let raw: Value = serde_json::from_str(&stored).unwrap();
            assert!(raw.get("message").is_none());
            assert!(raw["proto"].get("message_bytes").is_none());
            assert_eq!(
                decoded["projection_extensions"]["future_projection"],
                json!([1, "雪"])
            );
        }
        let projected = s.ok(&[
            "get-v2", "--file", &file, "--ref", record_ref, "--view", "decoded",
        ]);
        assert_eq!(
            projected["source_schema_version"],
            doc.header.schema_version
        );
        assert_eq!(
            projected["projection_profile"],
            ciallogcat::protobuf::PROJECTION_PROFILE
        );
        let invalid_offset = decoded_text.find('雪').unwrap() + 1;
        assert_eq!(
            s.fail(&[
                "get-v2",
                "--file",
                &file,
                "--ref",
                record_ref,
                "--view",
                "decoded",
                "--text-offset",
                &invalid_offset.to_string()
            ]),
            "invalid_arguments"
        );
        assert_eq!(
            s.fail(&[
                "get-v2", "--file", &file, "--ref", record_ref, "--view", "raw"
            ]),
            "invalid_arguments"
        );
        let context = s.ok(&[
            "context-v2",
            "--file",
            &file,
            "--ref",
            record_ref,
            "--max-response-bytes",
            "8192",
        ]);
        assert_eq!(context["source_schema_version"], doc.header.schema_version);
        assert_eq!(records(&context), [1, 2]);
        let marker = s.ok(&[
            "get-v2",
            "--file",
            &file,
            "--ref",
            reference(&context, 1),
            "--view",
            "decoded",
        ]);
        let marker: Value =
            serde_json::from_str(marker["record_json_chunk"].as_str().unwrap()).unwrap();
        assert_eq!(marker["type"], "event");
        assert_eq!(marker["message"], "marker");
    }
}

#[test]
fn compact_nul_payload_decodes_beyond_archive_line_limit_with_resumable_bounded_responses() {
    let s = Sandbox::new();
    let message = vec![0; ciallogcat::protobuf::MAX_FRAME_BYTES as usize - 128];
    let (doc, bytes) = proto_document(&message, NativeFormat::CompactV3);
    let file = s.path("large.jsonl");
    fs::write(&file, bytes).unwrap();
    let page = s.ok(&["query-v2", "--file", &file, "--preview-bytes", "0"]);
    let text = get_chunks(&s, &file, reference(&page, 0), "decoded", 1048576);
    assert!(text.len() > MAX_RECORD_BYTES as usize);
    let decoded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(decoded["message"].as_str().unwrap().as_bytes(), message);
    assert!(
        document::encode_record_projection(
            &doc.header,
            &doc.entries[0],
            &doc.record_metadata[0],
            8192
        )
        .is_err()
    );
}

#[test]
fn compact_query_rejects_bad_profiles_mixed_schemas_and_competing_evidence() {
    let s = Sandbox::new();
    let (_, bytes) = proto_document(b"valid", NativeFormat::CompactV3);
    let lines: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    let file = s.path("bad.jsonl");
    for mutation in [
        "profile", "encoding", "schema", "message", "proto", "payload", "footer",
    ] {
        let mut lines = lines.clone();
        match mutation {
            "profile" => lines[0]["normalization"]["proto_projection"] = "future".into(),
            "encoding" => lines[0]["record_encoding"] = "future".into(),
            "schema" => lines[1]["schema_version"] = 2.into(),
            "message" => lines[1]["message"] = "forged".into(),
            "proto" => lines[1]["proto"]["pid"] = 4.into(),
            "payload" => lines[1]["proto"]["raw_payload"] = "gA==".into(),
            _ => lines.last_mut().unwrap()["schema_version"] = 2.into(),
        }
        let bytes: Vec<u8> = lines
            .iter()
            .flat_map(|v| {
                let mut b = serde_json::to_vec(v).unwrap();
                b.push(b'\n');
                b
            })
            .collect();
        fs::write(&file, bytes).unwrap();
        assert_eq!(
            s.fail(&["query-v2", "--file", &file]),
            "invalid_source",
            "{mutation}"
        );
    }
}

#[test]
fn decoded_legacy_retains_extensions_and_compact_session_requires_matching_schema() {
    let s = Sandbox::new();
    let dir = s.session(&[(1, "legacy 雪")]);
    let file = s.path("session/logs.jsonl");
    let mut legacy: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    legacy["future"] = json!({"items":[42,"雪"]});
    fs::write(&file, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let page = s.ok(&["query-v2", "--file", &file]);
    let decoded = s.ok(&[
        "get-v2",
        "--file",
        &file,
        "--ref",
        reference(&page, 0),
        "--view",
        "decoded",
    ]);
    assert_eq!(decoded["source_schema_version"], 1);
    assert_eq!(
        serde_json::from_str::<Value>(decoded["record_json_chunk"].as_str().unwrap()).unwrap(),
        legacy
    );
    let (doc, bytes) = proto_document(b"compact", NativeFormat::CompactV3);
    fs::write(&file, &bytes).unwrap();
    s.native_status(&doc.header, 2, bytes.len(), 2);
    let page = s.ok(&["query-v2", "--session-dir", &dir]);
    let status_path = s.root.join("session/status.json");
    let mut status: Value = serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    status["record_schema_version"] = 2.into();
    fs::write(status_path, serde_json::to_vec(&status).unwrap()).unwrap();
    assert_eq!(
        s.fail(&["query-v2", "--session-dir", &dir]),
        "invalid_source"
    );
    assert_eq!(
        s.fail(&["query-v2", "--session-dir", &dir, "--cursor", cursor(&page)]),
        "stale_cursor"
    );
}
