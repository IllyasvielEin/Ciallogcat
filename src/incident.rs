//! Small, explainable log signatures. A match is a navigation hint, not a diagnosis.
use crate::model::{Level, LogEntry, LogEntryKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hint {
    JavaFatal,
    NativeSignal,
    NativeTombstone,
    Anr,
}

impl Hint {
    pub fn label(self) -> &'static str {
        match self {
            Self::JavaFatal => "Java 崩溃线索",
            Self::NativeSignal => "Native signal 线索",
            Self::NativeTombstone => "Native tombstone 线索",
            Self::Anr => "ANR 线索",
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::JavaFatal => "AndroidRuntime，E/F，消息以 FATAL EXCEPTION: 开头",
            Self::NativeSignal => "libc，F，消息以 Fatal signal 开头",
            Self::NativeTombstone => "DEBUG，F，消息以 *** *** *** 开头",
            Self::Anr => "ActivityManager，E/F，消息以 ANR in 开头",
        }
    }
}

pub fn classify(entry: &LogEntry) -> Option<Hint> {
    if entry.kind != LogEntryKind::Log {
        return None;
    }
    // Inspect a bounded prefix, never scan an arbitrary multi-megabyte body
    // during indexing. Full text remains untouched for reading and copying.
    let mut end = entry.message.len().min(256);
    while !entry.message.is_char_boundary(end) {
        end -= 1;
    }
    let message = entry.message[..end].trim_start();
    match (entry.tag.as_ref(), entry.level) {
        ("AndroidRuntime", Level::Error | Level::Fatal)
            if message.starts_with("FATAL EXCEPTION:") =>
        {
            Some(Hint::JavaFatal)
        }
        ("libc", Level::Fatal) if message.starts_with("Fatal signal ") => Some(Hint::NativeSignal),
        ("DEBUG", Level::Fatal) if message.starts_with("*** *** ***") => {
            Some(Hint::NativeTombstone)
        }
        ("ActivityManager", Level::Error | Level::Fatal) if message.starts_with("ANR in ") => {
            Some(Hint::Anr)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_require_structured_fields_and_preserve_original_evidence() {
        for (tag, level, text, hint) in [
            (
                "AndroidRuntime",
                Level::Error,
                "FATAL EXCEPTION: main\njava.lang.IllegalStateException",
                Hint::JavaFatal,
            ),
            (
                "libc",
                Level::Fatal,
                "Fatal signal 11 (SIGSEGV)",
                Hint::NativeSignal,
            ),
            (
                "DEBUG",
                Level::Fatal,
                "*** *** *** *** ***",
                Hint::NativeTombstone,
            ),
            (
                "ActivityManager",
                Level::Error,
                "ANR in com.example",
                Hint::Anr,
            ),
        ] {
            let entry = LogEntry::new("", 1, 1, tag, level, text);
            assert_eq!(classify(&entry), Some(hint));
            assert_eq!(entry.message.as_ref(), text);
            let mut wrong_tag = entry.clone();
            wrong_tag.tag = "AppDocumentation".into();
            assert_eq!(classify(&wrong_tag), None);
            let mut wrong_level = entry.clone();
            wrong_level.level = Level::Info;
            assert_eq!(classify(&wrong_level), None);
            assert_eq!(classify(&LogEntry::unparsed(text)), None);
        }
    }
}
