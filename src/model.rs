use std::sync::Arc;

use eframe::egui::Color32;
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

    pub fn color(self, dark: bool) -> Color32 {
        match (self, dark) {
            (Self::Verbose, true) => Color32::from_rgb(142, 151, 166),
            (Self::Debug, true) => Color32::from_rgb(112, 161, 255),
            (Self::Info, true) => Color32::from_rgb(101, 203, 152),
            (Self::Warn, true) => Color32::from_rgb(239, 190, 82),
            (Self::Error | Self::Fatal, true) => Color32::from_rgb(255, 111, 118),
            (Self::Verbose, false) => Color32::from_rgb(91, 100, 114),
            (Self::Debug, false) => Color32::from_rgb(45, 103, 196),
            (Self::Info, false) => Color32::from_rgb(28, 130, 79),
            (Self::Warn, false) => Color32::from_rgb(166, 102, 0),
            (Self::Error | Self::Fatal, false) => Color32::from_rgb(194, 48, 55),
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

    pub fn first_line(&self) -> &str {
        self.message.lines().next().unwrap_or_default()
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

    pub fn copy_text(&self) -> String {
        let process = self.process_name();
        format!(
            "{} {:>5} {:>5} {} {:<24} {}: {}",
            self.time,
            self.pid,
            self.tid,
            self.level.short(),
            process,
            self.tag,
            self.message
        )
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
