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

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub time: Arc<str>,
    pub pid: u32,
    pub tid: u32,
    pub process: Option<Arc<str>>,
    pub tag: Arc<str>,
    pub level: Level,
    pub message: Arc<str>,
}

impl LogEntry {
    /// Conservative owned-text and per-row index/accounting cost. LogStore
    /// separately accounts for the capacity of its entry allocations.
    pub fn payload_bytes(&self) -> usize {
        self.time.len()
            + self.tag.len()
            + self.message.len()
            + self.process.as_ref().map_or(0, |process| process.len())
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
            time: time.into(),
            pid,
            tid,
            process: None,
            tag: tag.into(),
            level,
            message: message.into(),
        }
    }

    pub fn marker(message: impl Into<Arc<str>>) -> Self {
        Self::new("", 0, 0, "logcat", Level::Info, message)
    }

    pub fn process_name(&self) -> &str {
        self.process.as_deref().unwrap_or("—")
    }

    pub fn matches_package(&self, package: &str) -> bool {
        if package.is_empty() {
            return true;
        }
        let Some(process) = self.process.as_deref() else {
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
        let process = self.process_name();
        [
            self.time.len(),
            digits(self.pid).max(5),
            digits(self.tid).max(5),
            self.level.short().len(),
            process.len(),
            24usize.saturating_sub(process.chars().take(24).count()),
            self.tag.len(),
            self.message.len(),
            7, // Fixed spaces and the tag's colon-space separator.
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }

    pub fn append_copy_text(&self, text: &mut String) {
        use std::fmt::Write;
        let process = self.process_name();
        write!(
            text,
            "{} {:>5} {:>5} {} {:<24} {}: {}",
            self.time,
            self.pid,
            self.tid,
            self.level.short(),
            process,
            self.tag,
            self.message
        )
        .expect("writing to a String cannot fail");
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SavedFilter {
    pub name: String,
    pub package: String,
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
