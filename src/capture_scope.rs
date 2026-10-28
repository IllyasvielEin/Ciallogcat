//! Validated device history requests, shared by GUI and CLI. These limits are
//! independent of host duration, retained/output bytes and admitted records.
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::model::LogBuffer;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeError(String);

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ScopeError {}

/// Logcat's count is a positive signed 32-bit integer, not a host record budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct RecordCount(u32);

impl TryFrom<u32> for RecordCount {
    type Error = ScopeError;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if (1..=i32::MAX as u32).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ScopeError(
                "history count must be between 1 and 2147483647".into(),
            ))
        }
    }
}
impl From<RecordCount> for u32 {
    fn from(value: RecordCount) -> Self {
        value.0
    }
}
impl FromStr for RecordCount {
    type Err = ScopeError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ScopeError(
                "history count must contain only ASCII digits".into(),
            ));
        }
        value
            .parse::<u32>()
            .map_err(|_| ScopeError("history count is too large".into()))?
            .try_into()
    }
}

/// An exact device-time argument, never interpreted in the host timezone.
/// Unix seconds require a decimal point so logcat cannot treat them as a count.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceTime(String);

impl DeviceTime {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<DeviceTime> for String {
    fn from(value: DeviceTime) -> Self {
        value.0
    }
}
impl TryFrom<String> for DeviceTime {
    type Error = ScopeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl FromStr for DeviceTime {
    type Err = ScopeError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            ScopeError("time must be MM-DD HH:MM:SS.fraction, YYYY-MM-DD HH:MM:SS.fraction, or Unix SECONDS.fraction (1..=9 fractional digits)".into())
        };
        // Bound before parsing or allocation, and reject every shell/option
        // metacharacter, non-ASCII digit and embedded newline.
        if value.len() > 29
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || b"- :.".contains(&b))
        {
            return Err(invalid());
        }
        let (whole, fraction) = value.split_once('.').ok_or_else(invalid)?;
        if !(1..=9).contains(&fraction.len()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        if whole.bytes().all(|b| b.is_ascii_digit()) {
            whole.parse::<i64>().map_err(|_| invalid())?;
        } else {
            let (date, time) = whole.split_once(' ').ok_or_else(invalid)?;
            let fields: Vec<_> = date.split('-').collect();
            let (year, month, day) = match fields.as_slice() {
                [month, day] => (None, *month, *day),
                [year, month, day] if year.len() == 4 => (Some(*year), *month, *day),
                _ => return Err(invalid()),
            };
            let digits = |s: &str, len: usize| -> Result<u32, ScopeError> {
                if s.len() != len || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                s.parse().map_err(|_| invalid())
            };
            let year = year.map(|y| digits(y, 4)).transpose()?;
            if year == Some(0) {
                return Err(invalid());
            }
            let month = digits(month, 2)?;
            let day = digits(day, 2)?;
            let days = match month {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                2 if year.is_none_or(|y| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)) => 29,
                2 => 28,
                _ => return Err(invalid()),
            };
            let parts: Vec<_> = time.split(':').collect();
            let [hour, minute, second] = parts.as_slice() else {
                return Err(invalid());
            };
            if !(1..=days).contains(&day)
                || digits(hour, 2)? > 23
                || digits(minute, 2)? > 59
                || digits(second, 2)? > 59
            {
                return Err(invalid());
            }
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HistoryStart {
    Tail(RecordCount),
    Since(DeviceTime),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureScope {
    Follow { start: HistoryStart },
    Dump { start: Option<HistoryStart> },
}

impl Default for CaptureScope {
    fn default() -> Self {
        Self::Follow {
            start: HistoryStart::Tail(RecordCount(1)),
        }
    }
}
impl CaptureScope {
    pub fn from_inputs(
        dump: bool,
        tail: Option<&str>,
        since: Option<&str>,
    ) -> Result<Self, ScopeError> {
        let start = match (tail, since) {
            (Some(_), Some(_)) => {
                return Err(ScopeError(
                    "history count and time are mutually exclusive".into(),
                ));
            }
            (Some(n), None) => Some(HistoryStart::Tail(n.parse()?)),
            (None, Some(t)) => Some(HistoryStart::Since(t.parse()?)),
            (None, None) => None,
        };
        Ok(if dump {
            Self::Dump { start }
        } else {
            Self::Follow {
                start: start.unwrap_or(HistoryStart::Tail(RecordCount(1))),
            }
        })
    }

    pub fn is_dump(&self) -> bool {
        matches!(self, Self::Dump { .. })
    }

    /// Separate argv words. Shell callers must still quote each word.
    pub fn logcat_args(&self) -> Vec<String> {
        let (flag, start) = match self {
            Self::Follow { start } => ("-T", start),
            Self::Dump { start: Some(start) } => ("-t", start),
            Self::Dump { start: None } => return vec!["-d".into()],
        };
        let value = match start {
            HistoryStart::Tail(count) => count.0.to_string(),
            HistoryStart::Since(time) => time.0.clone(),
        };
        vec![flag.into(), value]
    }
}

/// Comma-separated, exact buffer names. Empty tokens and duplicates are likely
/// caller mistakes; `all` must stand alone. Defaults use LogBuffer::defaults().
pub fn parse_buffers(value: &str) -> Result<Vec<LogBuffer>, ScopeError> {
    let mut buffers = Vec::new();
    for name in value.split(',') {
        let buffer = LogBuffer::CHOICES
            .into_iter()
            .find(|b| b.name() == name)
            .ok_or_else(|| {
                ScopeError("buffers must be main,system,crash,events,radio or all".into())
            })?;
        if buffers.contains(&buffer) {
            return Err(ScopeError("duplicate buffer".into()));
        }
        buffers.push(buffer);
    }
    if buffers.len() > 1 && buffers.contains(&LogBuffer::All) {
        return Err(ScopeError("buffer all must be used alone".into()));
    }
    LogBuffer::normalize(&mut buffers);
    Ok(buffers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_validation_rejects_ambiguous_counts_impossible_dates_and_injection() {
        for value in [
            "0.0",
            "1792742400.123456789",
            "02-29 23:59:59.9",
            "2000-02-29 00:00:00.000",
        ] {
            assert_eq!(value.parse::<DeviceTime>().unwrap().as_str(), value);
        }
        for value in [
            "1792742400",
            ".1",
            "1.",
            "-1.0",
            "+1.0",
            "1e9.0",
            "1.0000000000",
            "9223372036854775808.0",
            "2025-02-29 00:00:00.0",
            "1900-02-29 00:00:00.0",
            "04-31 00:00:00.0",
            "13-01 00:00:00.0",
            "01-01 24:00:00.0",
            "01-01 00:60:00.0",
            "01-01 00:00:60.0",
            "1.0;id",
            "$(id)",
            "1.0\n",
            "1.0 -c",
            "--clear",
            "１.0",
            " 1.0",
        ] {
            assert!(value.parse::<DeviceTime>().is_err(), "{value:?}");
        }
    }

    #[test]
    fn deserialization_cannot_bypass_validated_values() {
        for value in ["0", "2147483648", "4294967296", "-1", "1.0", "\"1\""] {
            assert!(serde_json::from_str::<RecordCount>(value).is_err());
        }
        assert!(serde_json::from_str::<DeviceTime>("\"1;id\"").is_err());
        assert!(
            serde_json::from_str::<CaptureScope>(
                r#"{"mode":"dump","start":{"kind":"since","value":"1000"}}"#
            )
            .is_err()
        );
        for value in ["+1", "-1", "1 ", "", "０", "0", "2147483648"] {
            assert!(value.parse::<RecordCount>().is_err(), "{value}");
        }
    }

    #[test]
    fn mutually_exclusive_history_and_strict_buffers() {
        assert!(CaptureScope::from_inputs(false, Some("1"), Some("1.0")).is_err());
        assert_eq!(
            parse_buffers("crash,main").unwrap(),
            [LogBuffer::Main, LogBuffer::Crash]
        );
        for value in [
            "",
            "main,",
            "main,main",
            "all,main",
            "main;id",
            " main",
            "-c",
        ] {
            assert!(parse_buffers(value).is_err(), "{value}");
        }
    }
}
