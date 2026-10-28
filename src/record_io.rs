//! Bounded reading and writing of the existing CLI v1 JSONL exchange format.
//!
//! V1 does not preserve process attribution, source identity, record kind or
//! capture completeness. New captures may include a host observation timestamp;
//! older records omit it. Reading must not invent missing fields. The native
//! versioned document format will be a separate adapter, not a silent v1 change.
use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, Read};

use serde::{Deserialize, Serialize};

use crate::model::{Level, LogEntry};

/// Limit on encoded JSON bytes, including an optional newline. This is larger
/// than the ADB text limit because JSON escaping can expand source bytes.
pub const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
pub struct LegacyRecord {
    schema_version: u8,
    #[serde(rename = "type")]
    kind: String,
    pub seq: u64,
    /// Optional host receive time; absent in older v1 records. Never device time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_unix_ms: Option<u64>,
    pub time: String,
    pub pid: u32,
    pub tid: u32,
    pub level: String,
    pub tag: String,
    pub message: String,
    // Preserve optional extensions when an existing v1 query reads and emits
    // a record. Their meaning is deliberately not interpreted by this codec.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl LegacyRecord {
    /// Keep the legacy fields/type and preserve an existing host observation.
    /// Never assign an import/export time when the observation is absent.
    /// Marker/unparsed provenance cannot be recovered from a v1 artifact.
    pub fn from_entry(entry: &LogEntry, seq: u64) -> Self {
        Self {
            schema_version: 1,
            kind: "entry".into(),
            seq,
            observed_unix_ms: entry
                .provenance
                .and_then(|identity| identity.observed_unix_ms),
            time: entry.time.to_string(),
            pid: entry.pid,
            tid: entry.tid,
            level: entry.level.short().into(),
            tag: entry.tag.to_string(),
            message: entry.message.to_string(),
            extra: BTreeMap::new(),
        }
    }

    fn valid(&self) -> bool {
        self.schema_version == 1
            && self.kind == "entry"
            && self.seq != 0
            && matches!(self.level.as_str(), "V" | "D" | "I" | "W" | "E" | "F")
    }

    pub fn level(&self) -> Option<Level> {
        (self.level.len() == 1)
            .then(|| self.level.chars().next().and_then(Level::from_letter))
            .flatten()
    }
}

#[derive(Clone, Copy)]
pub enum InputMode {
    /// Explicit offline input: JSON Lines permits a valid final JSON value
    /// without a newline. This does not establish capture completeness.
    Offline,
    /// An active writer publishes only newline-terminated records.
    Active,
}

#[derive(Debug)]
pub struct LocatedRecord {
    pub record: LegacyRecord,
    pub offset: u64,
    pub encoded_bytes: u64,
}

#[derive(Debug)]
pub enum ReadOutcome {
    Record(LocatedRecord),
    End,
    /// EOF before a newline in Active mode. These bytes were not committed.
    Incomplete,
}

#[derive(Debug)]
pub struct RecordError {
    pub kind: RecordErrorKind,
    pub line: u64,
    pub offset: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordErrorKind {
    Io,
    InvalidData,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, byte {}: {}",
            self.line, self.offset, self.message
        )
    }
}
impl std::error::Error for RecordError {}

pub struct LegacyReader<R> {
    reader: R,
    mode: InputMode,
    line: u64,
    offset: u64,
    previous_seq: u64,
    ended: bool,
    buffer: Vec<u8>,
}

impl<R: BufRead> LegacyReader<R> {
    pub fn new(reader: R, mode: InputMode) -> Self {
        Self {
            reader,
            mode,
            line: 1,
            offset: 0,
            previous_seq: 0,
            ended: false,
            buffer: Vec::new(),
        }
    }

    pub fn read_record(&mut self) -> Result<ReadOutcome, RecordError> {
        if self.ended {
            return Ok(ReadOutcome::End);
        }
        // Errors and incomplete tails terminate this reader. A caller must
        // explicitly open a new snapshot instead of skipping corrupt bytes.
        self.ended = true;
        self.buffer.clear();
        self.reader
            .by_ref()
            .take(MAX_RECORD_BYTES + 1)
            .read_until(b'\n', &mut self.buffer)
            .map_err(|error| RecordError {
                kind: RecordErrorKind::Io,
                ..self.error(error)
            })?;
        if self.buffer.is_empty() {
            return Ok(ReadOutcome::End);
        }
        if self.buffer.len() as u64 > MAX_RECORD_BYTES {
            return Err(self.error("record exceeds encoded size limit"));
        }
        if !self.buffer.ends_with(b"\n") && matches!(self.mode, InputMode::Active) {
            return Ok(ReadOutcome::Incomplete);
        }
        crate::json_input::reject_ambiguous_number_key(&self.buffer)
            .map_err(|error| self.error(error))?;
        let record: LegacyRecord =
            serde_json::from_slice(&self.buffer).map_err(|error| self.error(error))?;
        if !record.valid() || record.seq <= self.previous_seq {
            return Err(self.error("invalid v1 record schema or sequence"));
        }
        let offset = self.offset;
        let encoded_bytes = self.buffer.len() as u64;
        self.offset += encoded_bytes;
        self.line += 1;
        self.previous_seq = record.seq;
        self.ended = false;
        Ok(ReadOutcome::Record(LocatedRecord {
            record,
            offset,
            encoded_bytes,
        }))
    }

    fn error(&self, message: impl fmt::Display) -> RecordError {
        RecordError {
            kind: RecordErrorKind::InvalidData,
            line: self.line,
            offset: self.offset,
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn line(seq: u64, message: &str) -> Vec<u8> {
        let entry = LogEntry::new("10-23 12:34:56.789", 42, 43, "标签", Level::Error, message);
        let mut bytes = serde_json::to_vec(&LegacyRecord::from_entry(&entry, seq)).unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn record(reader: &mut LegacyReader<io::Cursor<Vec<u8>>>) -> LocatedRecord {
        match reader.read_record().unwrap() {
            ReadOutcome::Record(record) => record,
            _ => panic!("expected record"),
        }
    }

    #[test]
    fn unicode_multiline_and_sequence_gaps_round_trip_without_reclassification() {
        let first = line(7, "请求🦀\nsecond line\0");
        let second = line(19, "different retained range");
        let bytes = [first.as_slice(), second.as_slice()].concat();
        let mut reader = LegacyReader::new(io::Cursor::new(bytes), InputMode::Offline);
        let a = record(&mut reader);
        assert_eq!(a.record.seq, 7);
        assert_eq!(a.record.message, "请求🦀\nsecond line\0");
        assert_eq!(a.record.level(), Some(Level::Error));
        assert_eq!(a.encoded_bytes, first.len() as u64);
        let b = record(&mut reader);
        assert_eq!(b.offset, first.len() as u64);
        assert_eq!(b.record.seq, 19);
        assert!(matches!(reader.read_record().unwrap(), ReadOutcome::End));
    }

    #[test]
    fn final_newline_depends_on_explicit_input_mode() {
        let mut bytes = line(1, "valid final value");
        bytes.pop();
        let mut offline = LegacyReader::new(io::Cursor::new(bytes.clone()), InputMode::Offline);
        assert_eq!(record(&mut offline).record.seq, 1);
        let mut active = LegacyReader::new(io::Cursor::new(bytes), InputMode::Active);
        assert!(matches!(
            active.read_record().unwrap(),
            ReadOutcome::Incomplete
        ));
        let mut broken = LegacyReader::new(io::Cursor::new(b"{\"seq\":1"), InputMode::Offline);
        assert!(broken.read_record().is_err());
    }

    #[test]
    fn unknown_optional_fields_survive_query_reserialization() {
        let mut value: serde_json::Value = serde_json::from_slice(&line(1, "legacy")).unwrap();
        value["extension"] = serde_json::json!({"source": ["未知", 7], "optional": null});
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        let mut reader = LegacyReader::new(io::Cursor::new(bytes), InputMode::Active);
        assert_eq!(
            serde_json::to_value(record(&mut reader).record).unwrap(),
            value
        );
    }

    #[test]
    fn corrupt_schema_or_sequence_reports_location_and_cannot_be_skipped() {
        for invalid in [
            b"{bad json}\n".to_vec(),
            b"\n".to_vec(),
            line(1, "duplicate sequence"),
            line(0, "zero sequence"),
            String::from_utf8(line(2, "wrong version"))
                .unwrap()
                .replace("\"schema_version\":1", "\"schema_version\":2")
                .into_bytes(),
            String::from_utf8(line(2, "wrong level"))
                .unwrap()
                .replace("\"level\":\"E\"", "\"level\":\"unknown\"")
                .into_bytes(),
        ] {
            let first = line(1, "ok");
            let bytes = [
                first.as_slice(),
                invalid.as_slice(),
                line(3, "later").as_slice(),
            ]
            .concat();
            let mut reader = LegacyReader::new(io::Cursor::new(bytes), InputMode::Offline);
            record(&mut reader);
            let error = reader.read_record().expect_err("invalid record");
            assert_eq!(error.line, 2);
            assert_eq!(error.offset, first.len() as u64);
            assert!(matches!(reader.read_record().unwrap(), ReadOutcome::End));
        }
    }

    #[test]
    fn encoded_limit_allows_source_escape_expansion_and_bounds_bad_input() {
        let message = "\0".repeat(1024 * 1024);
        let bytes = line(1, &message);
        assert!(bytes.len() > 6 * 1024 * 1024);
        let mut reader = LegacyReader::new(io::Cursor::new(bytes), InputMode::Offline);
        assert_eq!(record(&mut reader).record.message, message);

        struct Infinite {
            read: u64,
        }
        impl Read for Infinite {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                bytes.fill(b'x');
                self.read += bytes.len() as u64;
                Ok(bytes.len())
            }
        }
        let mut input = Infinite { read: 0 };
        {
            let mut reader = LegacyReader::new(io::BufReader::new(&mut input), InputMode::Offline);
            assert!(
                reader
                    .read_record()
                    .unwrap_err()
                    .message
                    .contains("size limit")
            );
        }
        // A BufReader may prefetch one buffer; the decoder never grows with
        // an unterminated/untrusted stream beyond its encoded record limit.
        assert!(input.read <= MAX_RECORD_BYTES + 8192);
    }
    #[test]
    fn optional_observation_is_typed_and_never_invented_on_read_or_export() {
        let original: serde_json::Value = serde_json::from_slice(&line(1, "legacy")).unwrap();
        assert!(original.get("observed_unix_ms").is_none());
        for time in [0, 1_700_000_000_123, u64::MAX] {
            let mut value = original.clone();
            value["observed_unix_ms"] = time.into();
            value["future_extension"] = serde_json::json!({"untouched":true});
            let bytes = serde_json::to_vec(&value).unwrap();
            let mut reader = LegacyReader::new(io::Cursor::new(bytes), InputMode::Offline);
            let r = record(&mut reader).record;
            assert_eq!(r.observed_unix_ms, Some(time));
            assert_eq!(serde_json::to_value(r).unwrap(), value);
        }
        let mut null = original.clone();
        null["observed_unix_ms"] = serde_json::Value::Null;
        let r: LegacyRecord = serde_json::from_value(null).unwrap();
        assert_eq!(r.observed_unix_ms, None);
        assert_eq!(serde_json::to_value(r).unwrap(), original);
        for invalid in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("123"),
            serde_json::json!(true),
            serde_json::json!({}),
            serde_json::from_str("18446744073709551616").unwrap(),
        ] {
            let mut value = original.clone();
            value["observed_unix_ms"] = invalid;
            let mut reader = LegacyReader::new(
                io::Cursor::new(serde_json::to_vec(&value).unwrap()),
                InputMode::Offline,
            );
            assert_eq!(
                reader.read_record().unwrap_err().kind,
                RecordErrorKind::InvalidData
            );
        }
        let mut e = LogEntry::new("", 1, 1, "Tag", Level::Info, "message");
        e.provenance = Some(crate::session::RecordIdentity {
            session_id: crate::session::SessionId::new(),
            seq: 1,
            segment: 1,
            attempt: 1,
            observed_unix_ms: Some(123),
        });
        assert_eq!(LegacyRecord::from_entry(&e, 1).observed_unix_ms, Some(123));
        e.provenance.as_mut().unwrap().observed_unix_ms = None;
        assert_eq!(LegacyRecord::from_entry(&e, 1).observed_unix_ms, None);
    }
}
