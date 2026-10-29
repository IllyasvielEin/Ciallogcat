use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogBuffer {
    Main,
    System,
    Crash,
    Events,
    Radio,
    All,
}

impl LogBuffer {
    pub const CHOICES: [Self; 6] = [
        Self::Main,
        Self::System,
        Self::Crash,
        Self::Events,
        Self::Radio,
        Self::All,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::System => "system",
            Self::Crash => "crash",
            Self::Events => "events",
            Self::Radio => "radio",
            Self::All => "all",
        }
    }
    pub fn defaults() -> Vec<Self> {
        vec![Self::Main, Self::System, Self::Crash]
    }
    pub fn normalize(buffers: &mut Vec<Self>) {
        buffers.sort();
        buffers.dedup();
        if buffers.contains(&Self::All) {
            *buffers = vec![Self::All];
        }
        if buffers.is_empty() {
            *buffers = Self::defaults();
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Level {
    #[default]
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    pub const ALL: [Self; 6] = [
        Self::Verbose,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
        Self::Fatal,
    ];

    pub fn from_letter(value: char) -> Option<Self> {
        match value {
            'V' => Some(Self::Verbose),
            'D' => Some(Self::Debug),
            'I' => Some(Self::Info),
            'W' => Some(Self::Warn),
            'E' => Some(Self::Error),
            'F' | 'A' => Some(Self::Fatal),
            _ => None,
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Verbose => "V",
            Self::Debug => "D",
            Self::Info => "I",
            Self::Warn => "W",
            Self::Error => "E",
            Self::Fatal => "F",
        }
    }
}

/// Text Logcat records keep structural banners and undecodable input distinct.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogEntryKind {
    Log,
    Marker,
    Unparsed,
}

impl LogEntryKind {
    /// Stable presentation label, shared by UI, text export and CLI rendering.
    pub fn label(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Marker => "event",
            Self::Unparsed => "unparsed",
        }
    }
}

/// Log metadata exists only for successfully parsed log records. The internal
/// slots on `LogEntry` are not evidence for event/unparsed rows; consumers must
/// use this projection instead of exposing or filtering their placeholder values.
#[derive(Clone, Copy, Debug)]
pub struct LogFields<'a> {
    pub time: &'a str,
    pub pid: u32,
    pub tid: u32,
    pub level: Level,
    pub tag: &'a str,
    pub process: Option<&'a str>,
    pub process_source: Option<ProcessSource>,
}

impl<'a> LogFields<'a> {
    pub fn process_name(self) -> &'a str {
        self.process.unwrap_or("—")
    }
}

/// A sampled process name is a hint, not proof of a log's process instance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSource {
    PsSnapshot,
    DeviceProcessCache,
}

/// Original Android protobuf values, separate from lossy display fields.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtoEvidence {
    pub time_sec: u64,
    pub time_nsec: u64,
    pub uid: u64,
    pub pid: u64,
    pub tid: u64,
    pub priority: i32,
    #[serde(with = "binary_base64")]
    pub raw_payload: Vec<u8>,
    #[serde(with = "binary_base64")]
    pub tag_bytes: Vec<u8>,
    #[serde(with = "binary_base64")]
    pub message_bytes: Vec<u8>,
    pub process_name: Option<String>,
}

impl ProtoEvidence {
    pub fn memory_bytes(&self) -> usize {
        self.raw_payload.capacity()
            + self.tag_bytes.capacity()
            + self.message_bytes.capacity()
            + self.process_name.as_ref().map_or(0, String::capacity)
            + std::mem::size_of::<Self>()
    }
}

pub(crate) mod binary_base64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let limit = crate::protobuf::MAX_FRAME_BYTES as usize;
        if encoded.len() > limit.div_ceil(3) * 4 {
            return Err(serde::de::Error::custom(
                "proto base64 exceeds 1 MiB field/frame limit",
            ));
        }
        let bytes = STANDARD.decode(encoded).map_err(serde::de::Error::custom)?;
        if bytes.len() > limit {
            return Err(serde::de::Error::custom(
                "proto bytes exceed 1 MiB field/frame limit",
            ));
        }
        Ok(bytes)
    }
}

/// Loss observed by the host text adapter. False does not establish that the
/// device produced complete data; None on a row means these facts are unknown.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TextLoss {
    pub truncated: bool,
    pub invalid_utf8_replaced: bool,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    /// Stable identity and host observation, absent for legacy/unowned input.
    pub provenance: Option<crate::session::RecordIdentity>,
    pub text_loss: Option<TextLoss>,
    pub proto: Option<Arc<ProtoEvidence>>,
    pub kind: LogEntryKind,
    pub time: Arc<str>,
    pub pid: u32,
    pub tid: u32,
    pub process: Option<Arc<str>>,
    pub process_source: Option<ProcessSource>,
    pub tag: Arc<str>,
    pub level: Level,
    pub message: Arc<str>,
}

impl LogEntry {
    pub fn log_fields(&self) -> Option<LogFields<'_>> {
        (self.kind == LogEntryKind::Log).then_some(LogFields {
            time: &self.time,
            pid: self.pid,
            tid: self.tid,
            level: self.level,
            tag: &self.tag,
            process: self.process.as_deref(),
            process_source: self.process_source,
        })
    }

    /// Conservative owned-text and per-row index/accounting cost. LogStore
    /// separately accounts for the capacity of its entry allocations.
    pub fn payload_bytes(&self) -> usize {
        self.time.len()
            + self.tag.len()
            + self.message.len()
            + self.process.as_ref().map_or(0, |process| process.len())
            + self.proto.as_ref().map_or(0, |proto| proto.memory_bytes())
            + 192
    }

    pub fn new(
        time: impl Into<Arc<str>>,
        pid: u32,
        tid: u32,
        tag: impl Into<Arc<str>>,
        level: Level,
        message: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            provenance: None,
            text_loss: None,
            proto: None,
            kind: LogEntryKind::Log,
            time: time.into(),
            pid,
            tid,
            process: None,
            process_source: None,
            tag: tag.into(),
            level,
            message: message.into(),
        }
    }

    pub fn marker(message: impl Into<Arc<str>>) -> Self {
        Self {
            kind: LogEntryKind::Marker,
            ..Self::new("", 0, 0, "logcat", Level::Info, message)
        }
    }

    pub fn unparsed(message: impl Into<Arc<str>>) -> Self {
        Self {
            kind: LogEntryKind::Unparsed,
            ..Self::new("", 0, 0, "logcat", Level::Info, message)
        }
    }

    /// Counts only successfully parsed logs whose process name is unknown.
    pub fn is_unresolved_process(&self) -> bool {
        self.log_fields()
            .is_some_and(|fields| fields.process.is_none())
    }

    pub fn process_name(&self) -> &str {
        self.log_fields().map_or("—", LogFields::process_name)
    }

    pub fn matches_package(&self, package: &str) -> bool {
        if package.is_empty() {
            return true;
        }
        let Some(process) = self.log_fields().and_then(|fields| fields.process) else {
            return false;
        };
        process == package
            || process
                .strip_prefix(package)
                .is_some_and(|suffix| suffix.starts_with(':'))
    }

    /// Format one complete record using the same text as clipboard export.
    pub fn copy_text(&self) -> String {
        let mut text = String::new();
        self.append_copy_text(&mut text);
        text
    }

    /// Exact UTF-8 length of `append_copy_text`, without formatting allocations.
    /// Rust's minimum field width counts characters, rather than UTF-8 bytes.
    pub fn copy_text_bytes(&self) -> Option<usize> {
        fn digits(mut number: u32) -> usize {
            let mut count = 1;
            while number >= 10 {
                number /= 10;
                count += 1;
            }
            count
        }
        let Some(fields) = self.log_fields() else {
            return self
                .kind
                .label()
                .len()
                .checked_add(3)?
                .checked_add(self.message.len());
        };
        let process = fields.process_name();
        [
            fields.time.len(),
            digits(fields.pid).max(5),
            digits(fields.tid).max(5),
            fields.level.short().len(),
            process.len(),
            24usize.saturating_sub(process.chars().take(24).count()),
            fields.tag.len(),
            self.message.len(),
            7, // Fixed spaces and the tag's colon-space separator.
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }

    pub fn append_copy_text(&self, text: &mut String) {
        use std::fmt::Write;
        let Some(fields) = self.log_fields() else {
            write!(text, "[{}] {}", self.kind.label(), self.message)
                .expect("writing to a String cannot fail");
            return;
        };
        let process = fields.process_name();
        write!(
            text,
            "{} {:>5} {:>5} {} {:<24} {}: {}",
            fields.time,
            fields.pid,
            fields.tid,
            fields.level.short(),
            process,
            fields.tag,
            self.message
        )
        .expect("writing to a String cannot fail");
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SavedFilter {
    pub name: String,
    pub package: String,
    #[serde(default)]
    pub tag_include: String,
    #[serde(default)]
    pub tag_exclude: String,
    pub min_level: Level,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_byte_count_matches_formatter_unicode_padding_and_numeric_widths() {
        for process in [
            None,
            Some(""),
            Some("请求🦀worker"),
            Some("a very long process identity beyond padding"),
            Some("e\u{301}👩\u{200d}💻"),
        ] {
            for pid in [0, 7, 99_999, 100_000, u32::MAX] {
                for tid in [0, 7, 99_999, 100_000, u32::MAX] {
                    for message in ["", "a\r\nb\n", "🦀\0中", "full text"] {
                        let mut entry =
                            LogEntry::new("12:34", pid, tid, "标签", Level::Info, message);
                        entry.process = process.map(Arc::from);
                        assert_eq!(entry.copy_text_bytes(), Some(entry.copy_text().len()));
                    }
                }
            }
        }
    }

    #[test]
    fn structural_projection_and_copy_never_expose_placeholder_metadata() {
        for kind in [LogEntryKind::Marker, LogEntryKind::Unparsed] {
            for message in ["", "boundary", "雪\0\r\nnext\n\n", "tail  "] {
                let mut row = LogEntry::new("fake-time", 42, 43, "fake-tag", Level::Fatal, message);
                row.kind = kind;
                row.process = Some("fake.process".into());
                row.process_source = Some(ProcessSource::PsSnapshot);
                assert!(row.log_fields().is_none());
                assert_eq!(row.process_name(), "—");
                assert!(!row.is_unresolved_process());
                assert!(row.matches_package(""));
                assert!(!row.matches_package("fake.process"));
                assert_eq!(row.copy_text(), format!("[{}] {message}", kind.label()));
                assert_eq!(row.copy_text_bytes(), Some(row.copy_text().len()));
                assert_eq!(row.message.as_ref(), message);
            }
        }
        let row = LogEntry::new("real-time", 0, 0, "logcat", Level::Info, "real log");
        let fields = row.log_fields().unwrap();
        assert_eq!(
            (fields.pid, fields.tid, fields.level, fields.tag),
            (0, 0, Level::Info, "logcat")
        );
        assert!(row.copy_text().contains("    0     0 I"));
    }

    #[test]
    fn package_filter_includes_secondary_processes() {
        let mut entry = LogEntry::new("", 1, 1, "Tag", Level::Info, "message");
        entry.process = Some(Arc::from("com.example.app:sync"));

        assert!(entry.matches_package("com.example.app"));
        assert!(entry.matches_package("com.example.app:sync"));
        assert!(!entry.matches_package("com.example.other"));
    }

    #[test]
    fn level_letters_are_parsed() {
        assert_eq!(Level::from_letter('V'), Some(Level::Verbose));
        assert_eq!(Level::from_letter('A'), Some(Level::Fatal));
        assert_eq!(Level::from_letter('S'), None);
    }
}
