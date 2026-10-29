//! Deterministic native fixtures, including deliberately malformed row variants.
#![allow(dead_code)]
use ciallogcat::document::{CaptureStatus, DocumentHeader, StreamEncoder};
use ciallogcat::model::{Level, LogBuffer, LogEntry};
use ciallogcat::session::RecordIdentity;
use serde_json::Value;

pub fn header() -> DocumentHeader {
    let mut header = DocumentHeader::live(
        "12345678-1234-4234-8234-123456789abc".parse().unwrap(),
        "TEST".into(),
        vec![LogBuffer::Main],
    );
    header.artifact_id = "abcdefab-1234-4234-8234-123456789abc".parse().unwrap();
    header
}
pub fn row(seq: u64, message: &str, received: Option<u64>) -> Value {
    entry_row(
        seq,
        LogEntry::new("10-28 00:00:00.000", 42, 43, "AppTag", Level::Info, message),
        received,
    )
}
pub fn entry_row(seq: u64, mut entry: LogEntry, received: Option<u64>) -> Value {
    let header = header();
    entry.provenance = Some(RecordIdentity {
        session_id: header.session_id.unwrap(),
        seq,
        segment: 1,
        attempt: 1,
        observed_unix_ms: received,
    });
    let mut encoder = StreamEncoder::new(header).unwrap();
    serde_json::from_slice(&encoder.record_bytes(&entry).unwrap()).unwrap()
}
pub fn line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}
pub fn header_bytes() -> Vec<u8> {
    line(&serde_json::to_value(header()).unwrap())
}
pub fn footer(rows: &[Value]) -> Vec<u8> {
    let encoder = StreamEncoder::new(header()).unwrap();
    let mut end: Value =
        serde_json::from_slice(&encoder.finish_bytes(CaptureStatus::default()).unwrap()).unwrap();
    let first = rows.first().and_then(|r| r["seq"].as_u64());
    let last = rows.last().and_then(|r| r["seq"].as_u64());
    end["record_count"] = (rows.len() as u64).into();
    end["first_seq"] = first.into();
    end["last_seq"] = last.into();
    end["scope"]["source_first_seq"] = first.into();
    end["scope"]["source_last_seq"] = last.into();
    end["scope"]["source_record_count"] = (rows.len() as u64).into();
    line(&end)
}
pub fn bytes(rows: &[Value], finalized: bool) -> Vec<u8> {
    let mut bytes = header_bytes();
    for row in rows {
        bytes.extend(line(row));
    }
    if finalized {
        bytes.extend(footer(rows));
    }
    bytes
}
pub fn set_status(status: &mut Value, seq: u64, bytes: u64) {
    let header = header();
    status["record_schema_version"] = header.schema_version.into();
    status["artifact_id"] = header.artifact_id.to_string().into();
    status["session_id"] = header.session_id.unwrap().to_string().into();
    status["visible_end"] = serde_json::json!({"seq":seq,"offset":bytes});
    status["entries"] = seq.into();
    status["bytes"] = bytes.into();
}
