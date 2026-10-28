//! Human-readable CLI rendering, deliberately separate from archive projection
//! and desktop clipboard formatting. Every displayed time names its clock.
use std::collections::BTreeMap;
use std::io::Write;

use ciallogcat::model::{LogEntry, LogEntryKind};
use jiff::{Timestamp, Zoned, tz::TimeZone};

use super::{Result, io_error, usage};

#[derive(Clone, Copy, Eq, PartialEq)]
enum TimeFormat {
    Readable,
    Epoch,
    Rfc3339,
}

pub(super) struct TextOptions {
    format: TimeFormat,
    zone: TimeZone,
    local_unavailable: bool,
}

#[derive(Clone, Copy)]
struct ObservedTime {
    seconds: u64,
    nanoseconds: u32,
    digits: u8,
}
impl ObservedTime {
    fn fraction(self) -> String {
        if self.digits == 3 {
            format!(".{:03}", self.nanoseconds / 1_000_000)
        } else {
            format!(".{:09}", self.nanoseconds)
        }
    }
    fn epoch(self) -> String {
        format!("{}{}", self.seconds, self.fraction())
    }
    fn timestamp(self) -> Option<Timestamp> {
        Timestamp::new(self.seconds.try_into().ok()?, self.nanoseconds as i32).ok()
    }
}

impl TextOptions {
    pub(super) fn parse(flags: &mut BTreeMap<String, String>) -> Result<Self> {
        let format = match flags
            .remove("--time-format")
            .as_deref()
            .unwrap_or("readable")
        {
            "readable" => TimeFormat::Readable,
            "epoch" => TimeFormat::Epoch,
            "rfc3339" => TimeFormat::Rfc3339,
            _ => return Err(usage("--time-format must be readable, epoch or rfc3339")),
        };
        let local = match flags.remove("--timezone").as_deref().unwrap_or("local") {
            "local" => true,
            "utc" => false,
            _ => return Err(usage("--timezone must be local or utc")),
        };
        // Resolve once per invocation, not per row/page. Epoch output has no
        // timezone, so it must not depend on an available system timezone.
        let (zone, local_unavailable) = if local && format != TimeFormat::Epoch {
            match TimeZone::try_system() {
                Ok(zone) if !zone.is_unknown() => (zone, false),
                _ => (TimeZone::UTC, true),
            }
        } else {
            (TimeZone::UTC, false)
        };
        Ok(Self {
            format,
            zone,
            local_unavailable,
        })
    }

    fn time_prefix(&self, entry: &LogEntry, legacy_received: Option<u64>, device: bool) -> String {
        let source = if device { "device" } else { "received" };
        let time = if device {
            entry.proto.as_ref().and_then(|p| {
                (p.time_nsec < 1_000_000_000).then_some(ObservedTime {
                    seconds: p.time_sec,
                    nanoseconds: p.time_nsec as u32,
                    digits: 9,
                })
            })
        } else {
            legacy_received
                .or_else(|| entry.provenance.and_then(|p| p.observed_unix_ms))
                .map(|ms| ObservedTime {
                    seconds: ms / 1000,
                    nanoseconds: (ms % 1000) as u32 * 1_000_000,
                    digits: 3,
                })
        };
        let Some(time) = time else {
            return format!("[{source}-time-unavailable; device-text={:?}]", entry.time);
        };
        if self.format == TimeFormat::Epoch {
            return format!("{} [{source}]", time.epoch());
        }
        let Some(timestamp) = time.timestamp() else {
            return format!("{} [{source}; calendar-unavailable]", time.epoch());
        };
        let mut zoned = timestamp.to_zoned(self.zone.clone());
        let mut note = if self.local_unavailable {
            "; local-timezone-unavailable; shown-in-utc"
        } else {
            ""
        };
        // RFC3339 cannot express historical offsets containing seconds. Never
        // round an offset and thereby silently change the represented instant.
        if self.format == TimeFormat::Rfc3339 && zoned.offset().seconds() % 60 != 0 {
            zoned = timestamp.to_zoned(TimeZone::UTC);
            note = "; local-offset-unrepresentable; shown-in-utc";
        }
        format!("{} [{source}{note}]", calendar(&zoned, time, self.format))
    }

    pub(super) fn write_record(
        &self,
        writer: &mut dyn Write,
        entry: &LogEntry,
        kind: &str,
        legacy_received: Option<u64>,
        device_time: bool,
    ) -> Result<()> {
        let time = self.time_prefix(entry, legacy_received, device_time);
        if entry.kind == LogEntryKind::Log {
            write!(
                writer,
                "{} {:>5} {:>5} {} {:<24} {}: ",
                time,
                entry.pid,
                entry.tid,
                entry.level.short(),
                entry.process_name(),
                entry.tag
            )
            .map_err(io_error)?;
        } else {
            write!(writer, "{time} [{kind}] ").map_err(io_error)?;
        }
        writer
            .write_all(entry.message.as_bytes())
            .map_err(io_error)?;
        // Preserve all internal whitespace and intentional trailing blank lines.
        // A record that already owns its LF/CRLF terminator gets no second LF.
        if !entry.message.ends_with('\n') {
            writer.write_all(b"\n").map_err(io_error)?;
        }
        Ok(())
    }
}

fn calendar(zoned: &Zoned, time: ObservedTime, format: TimeFormat) -> String {
    let separator = if format == TimeFormat::Rfc3339 {
        'T'
    } else {
        ' '
    };
    let date = format!(
        "{:04}-{:02}-{:02}{separator}{:02}:{:02}:{:02}{}",
        zoned.year(),
        zoned.month(),
        zoned.day(),
        zoned.hour(),
        zoned.minute(),
        zoned.second(),
        time.fraction()
    );
    let offset = zoned.offset().seconds();
    if offset == 0 && format == TimeFormat::Rfc3339 {
        return format!("{date}Z");
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let seconds = offset.unsigned_abs();
    if seconds.is_multiple_of(60) {
        format!(
            "{date}{sign}{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60
        )
    } else {
        format!(
            "{date}{sign}{:02}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciallogcat::model::{Level, ProtoEvidence};
    use ciallogcat::session::{RecordIdentity, SessionId};
    use std::sync::Arc;

    fn entry(message: &str) -> LogEntry {
        LogEntry::new("10-28 03:43:20.123", 12, 34, "Tag", Level::Info, message)
    }
    fn options(format: TimeFormat, zone: TimeZone) -> TextOptions {
        TextOptions {
            format,
            zone,
            local_unavailable: false,
        }
    }
    fn proto(seconds: u64, nanoseconds: u64) -> LogEntry {
        let mut entry = entry("hello");
        entry.proto = Some(Arc::new(ProtoEvidence {
            time_sec: seconds,
            time_nsec: nanoseconds,
            uid: 0,
            pid: 12,
            tid: 34,
            priority: 4,
            raw_payload: vec![],
            tag_bytes: b"Tag".to_vec(),
            message_bytes: b"hello".to_vec(),
            process_name: None,
        }));
        entry
    }
    fn parse(args: &[(&str, &str)]) -> Result<TextOptions> {
        TextOptions::parse(
            &mut args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn received_time_readable_rfc3339_and_epoch_keep_milliseconds() {
        let e = entry("hi");
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC).time_prefix(
                &e,
                Some(1_700_000_000_123),
                false
            ),
            "2023-11-14 22:13:20.123+00:00 [received]"
        );
        assert_eq!(
            options(TimeFormat::Rfc3339, TimeZone::UTC).time_prefix(
                &e,
                Some(1_700_000_000_123),
                false
            ),
            "2023-11-14T22:13:20.123Z [received]"
        );
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC).time_prefix(
                &e,
                Some(1_700_000_000_123),
                false
            ),
            "1700000000.123 [received]"
        );
    }

    #[test]
    fn device_nanoseconds_and_extreme_epoch_never_use_floating_point() {
        let e = proto(1_700_000_000, 1);
        assert_eq!(
            options(TimeFormat::Rfc3339, TimeZone::UTC).time_prefix(&e, Some(99), true),
            "2023-11-14T22:13:20.000000001Z [device]"
        );
        let e = proto(u64::MAX, 999_999_999);
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC).time_prefix(&e, None, true),
            "18446744073709551615.999999999 [device]"
        );
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC).time_prefix(&e, None, true),
            "18446744073709551615.999999999 [device; calendar-unavailable]"
        );
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC).time_prefix(&e, Some(u64::MAX), false),
            "18446744073709551.615 [received]"
        );
    }

    #[test]
    fn fixed_offsets_cross_day_and_year_without_changing_instant() {
        let positive = TimeZone::fixed(jiff::tz::Offset::from_seconds(8 * 3600).unwrap());
        let negative =
            TimeZone::fixed(jiff::tz::Offset::from_seconds(-5 * 3600 - 30 * 60).unwrap());
        let time = Some(1_704_067_199_999); // 2023-12-31T23:59:59.999Z
        assert_eq!(
            options(TimeFormat::Readable, positive).time_prefix(&entry(""), time, false),
            "2024-01-01 07:59:59.999+08:00 [received]"
        );
        assert_eq!(
            options(TimeFormat::Rfc3339, negative).time_prefix(&entry(""), time, false),
            "2023-12-31T18:29:59.999-05:30 [received]"
        );
    }

    #[test]
    fn second_precision_offsets_do_not_get_silently_rounded_for_rfc3339() {
        let zone = TimeZone::fixed(jiff::tz::Offset::from_seconds(30).unwrap());
        assert_eq!(
            options(TimeFormat::Readable, zone.clone()).time_prefix(&entry(""), Some(0), false),
            "1970-01-01 00:00:30.000+00:00:30 [received]"
        );
        assert_eq!(
            options(TimeFormat::Rfc3339, zone).time_prefix(&entry(""), Some(0), false),
            "1970-01-01T00:00:00.000Z [received; local-offset-unrepresentable; shown-in-utc]"
        );
    }

    #[test]
    fn missing_time_never_borrows_the_other_clock_or_guesses_a_year() {
        let mut e = proto(1_700_000_000, 2);
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC).time_prefix(&e, None, false),
            "[received-time-unavailable; device-text=\"10-28 03:43:20.123\"]"
        );
        e.proto = None;
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC).time_prefix(&e, Some(123), true),
            "[device-time-unavailable; device-text=\"10-28 03:43:20.123\"]"
        );
        e.time = "raw\nambiguous\"time".into();
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC).time_prefix(&e, None, true),
            "[device-time-unavailable; device-text=\"raw\\nambiguous\\\"time\"]"
        );
        let invalid = proto(0, 1_000_000_000);
        assert!(
            options(TimeFormat::Epoch, TimeZone::UTC)
                .time_prefix(&invalid, None, true)
                .contains("device-time-unavailable")
        );
    }

    #[test]
    fn native_and_legacy_receipt_time_share_the_clock_without_fabricated_identity() {
        let mut e = entry("");
        let text = options(TimeFormat::Epoch, TimeZone::UTC);
        e.provenance = Some(RecordIdentity {
            session_id: SessionId::new(),
            seq: 1,
            segment: 1,
            attempt: 1,
            observed_unix_ms: Some(1234),
        });
        assert_eq!(text.time_prefix(&e, None, false), "1.234 [received]");
        assert_eq!(text.time_prefix(&e, Some(5678), false), "5.678 [received]");
        e.provenance = None;
        assert_eq!(text.time_prefix(&e, Some(1234), false), "1.234 [received]");
        assert!(e.provenance.is_none());
    }

    #[test]
    fn newline_terminators_and_message_whitespace_are_not_duplicated_or_trimmed() {
        let text = options(TimeFormat::Epoch, TimeZone::UTC);
        for message in [
            "",
            "hello",
            "hello\n",
            "hello\r\n",
            "a\nb",
            "a\n\n",
            "a \t",
            "\n\n",
        ] {
            let e = entry(message);
            let mut bytes = Vec::new();
            text.write_record(&mut bytes, &e, "entry", Some(1000), false)
                .unwrap();
            let rendered = String::from_utf8(bytes).unwrap();
            let payload = rendered.split_once("Tag: ").unwrap().1;
            let expected = if message.ends_with('\n') {
                message.to_string()
            } else {
                format!("{message}\n")
            };
            assert_eq!(payload, expected);
            assert_eq!(&*e.message, message);
        }
    }

    #[test]
    fn non_log_records_keep_kind_without_placeholder_fields() {
        let text = options(TimeFormat::Epoch, TimeZone::UTC);
        let e = LogEntry::marker("boundary\n");
        let mut bytes = Vec::new();
        text.write_record(&mut bytes, &e, "event", Some(0), false)
            .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "0.000 [received] [event] boundary\n"
        );
    }

    #[test]
    fn local_unavailability_is_explicit_and_epoch_ignores_timezone() {
        let mut text = options(TimeFormat::Readable, TimeZone::UTC);
        text.local_unavailable = true;
        assert_eq!(
            text.time_prefix(&entry(""), Some(0), false),
            "1970-01-01 00:00:00.000+00:00 [received; local-timezone-unavailable; shown-in-utc]"
        );
        let utc = parse(&[("--time-format", "epoch"), ("--timezone", "utc")]).unwrap();
        let local = parse(&[("--time-format", "epoch"), ("--timezone", "local")]).unwrap();
        assert_eq!(
            utc.time_prefix(&entry(""), Some(u64::MAX), false),
            local.time_prefix(&entry(""), Some(u64::MAX), false)
        );
        assert!(parse(&[("--time-format", "other")]).is_err());
        assert!(parse(&[("--timezone", "+08:00")]).is_err());
    }
}
