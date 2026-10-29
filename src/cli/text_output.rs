//! Human-readable CLI rendering, deliberately separate from archive projection
//! and desktop clipboard formatting. Exceptional and non-default clocks are explicit.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::Write;

use ciallogcat::model::LogEntry;
use jiff::{Timestamp, Zoned, tz::TimeZone};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
    process_width: usize,
    process_names_truncated: Cell<u64>,
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
        let process_width = flags
            .remove("--process-width")
            .map(|value| {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(usage("--process-width must be 0..4096 (0 means unlimited)"));
                }
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|width| *width <= 4096)
                    .ok_or_else(|| usage("--process-width must be 0..4096 (0 means unlimited)"))
            })
            .transpose()?
            .unwrap_or(32);
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
            process_width,
            process_names_truncated: Cell::new(0),
        })
    }

    pub(super) fn process_width(&self) -> usize {
        self.process_width
    }

    pub(super) fn process_names_truncated(&self) -> u64 {
        self.process_names_truncated.get()
    }

    fn time_prefix(&self, entry: &LogEntry, device: bool) -> String {
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
            entry
                .provenance
                .and_then(|p| p.observed_unix_ms)
                .map(|ms| ObservedTime {
                    seconds: ms / 1000,
                    nanoseconds: (ms % 1000) as u32 * 1_000_000,
                    digits: 3,
                })
        };
        let Some(time) = time else {
            return entry.log_fields().map_or_else(
                || format!("[{source}-time-unavailable]"),
                |fields| format!("[{source}-time-unavailable; device-text={:?}]", fields.time),
            );
        };
        if self.format == TimeFormat::Epoch {
            return if device {
                format!("{} [device]", time.epoch())
            } else {
                time.epoch()
            };
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
        let time = calendar(&zoned, time, self.format);
        if device || !note.is_empty() {
            format!("{time} [{source}{note}]")
        } else {
            time
        }
    }

    pub(super) fn write_record(
        &self,
        writer: &mut dyn Write,
        entry: &LogEntry,
        device_time: bool,
    ) -> Result<()> {
        let time = self.time_prefix(entry, device_time);
        let mut truncated = false;
        if let Some(fields) = entry.log_fields() {
            let (process, was_truncated) = process_field(fields.process_name(), self.process_width);
            truncated = was_truncated;
            write!(
                writer,
                "{} {:>5} {:>5} {} {} {}: ",
                time,
                fields.pid,
                fields.tid,
                fields.level.short(),
                process,
                fields.tag
            )
            .map_err(io_error)?;
        } else {
            write!(writer, "{time} [{}] ", entry.kind.label()).map_err(io_error)?;
        }
        writer
            .write_all(entry.message.as_bytes())
            .map_err(io_error)?;
        // Preserve all internal whitespace and intentional trailing blank lines.
        // A record that already owns its LF/CRLF terminator gets no second LF.
        if !entry.message.ends_with('\n') {
            writer.write_all(b"\n").map_err(io_error)?;
        }
        if truncated {
            self.process_names_truncated
                .set(self.process_names_truncated.get() + 1);
        }
        Ok(())
    }
}

/// A deterministic display-column budget, not a byte/character limit. Keep
/// complete grapheme clusters (including combining marks and emoji sequences)
/// on both sides so renderer/process suffixes remain useful. Only this derived
/// display field changes: the original record and message stay untouched.
fn process_field(name: &str, width: usize) -> (String, bool) {
    if width == 0 {
        return (name.to_owned(), false);
    }
    let original_width = UnicodeWidthStr::width(name);
    if original_width <= width {
        return (
            format!("{name}{}", " ".repeat(width - original_width)),
            false,
        );
    }
    let left_budget = (width - 1) / 2;
    let right_budget = width - 1 - left_budget;
    let mut prefix_end = 0;
    let mut left_width = 0;
    for (offset, grapheme) in name.grapheme_indices(true) {
        let columns = UnicodeWidthStr::width(grapheme);
        if left_width + columns > left_budget {
            break;
        }
        left_width += columns;
        prefix_end = offset + grapheme.len();
    }
    let mut suffix_start = name.len();
    let mut right_width = 0;
    for (offset, grapheme) in name.grapheme_indices(true).rev() {
        let columns = UnicodeWidthStr::width(grapheme);
        if offset < prefix_end || right_width + columns > right_budget {
            break;
        }
        right_width += columns;
        suffix_start = offset;
    }
    let compact = format!("{}…{}", &name[..prefix_end], &name[suffix_start..]);
    let columns = UnicodeWidthStr::width(compact.as_str());
    (
        format!("{compact}{}", " ".repeat(width.saturating_sub(columns))),
        true,
    )
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
    fn received(mut entry: LogEntry, ms: Option<u64>) -> LogEntry {
        entry.provenance = Some(RecordIdentity {
            session_id: SessionId::new(),
            seq: 1,
            segment: 1,
            attempt: 1,
            observed_unix_ms: ms,
        });
        entry
    }
    fn options(format: TimeFormat, zone: TimeZone) -> TextOptions {
        TextOptions {
            format,
            zone,
            local_unavailable: false,
            process_width: 32,
            process_names_truncated: Cell::new(0),
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
    fn process_width_has_explicit_bounds_and_unlimited_escape_hatch() {
        assert_eq!(parse(&[]).unwrap().process_width(), 32);
        for width in ["0", "1", "32", "4096"] {
            assert_eq!(
                parse(&[("--process-width", width)])
                    .unwrap()
                    .process_width(),
                width.parse::<usize>().unwrap()
            );
        }
        for width in [
            "",
            "-1",
            "+1",
            "1.5",
            "4097",
            "9999999999999999999999999999",
            " 32",
        ] {
            assert!(parse(&[("--process-width", width)]).is_err());
        }
        let name = "org.chromium.content.app.SandboxedProcessService0:renderer42";
        assert_eq!(process_field(name, 0), (name.to_owned(), false));
        assert_eq!(process_field("short", 5), ("short".to_owned(), false));
        assert_eq!(process_field("short", 8), ("short   ".to_owned(), false));
        assert_eq!(process_field(name, 1), ("…".to_owned(), true));
    }

    #[test]
    fn process_middle_ellipsis_preserves_prefix_suffix_and_display_columns() {
        let name = "org.chromium.content.app.SandboxedProcessService0:renderer42";
        let (compact, truncated) = process_field(name, 32);
        assert!(truncated);
        assert_eq!(UnicodeWidthStr::width(compact.as_str()), 32);
        assert!(compact.starts_with("org.chromium."));
        assert!(compact.ends_with(":renderer42"));
        for name in [
            "应用程序名字非常长的沙盒进程:渲染器42",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
            "👨‍👩‍👧‍👦👨‍👩‍👧‍👦👨‍👩‍👧‍👦:renderer👩🏽‍💻🇨🇳",
        ] {
            for width in 1..=16 {
                let (compact, truncated) = process_field(name, width);
                assert_eq!(
                    UnicodeWidthStr::width(compact.as_str()),
                    width,
                    "{name:?}, {width}: {compact:?}"
                );
                if truncated {
                    let (prefix, suffix) = compact.trim_end().split_once('…').unwrap();
                    assert!(name.starts_with(prefix) && name.ends_with(suffix));
                    let boundaries: Vec<_> = name
                        .grapheme_indices(true)
                        .map(|(i, _)| i)
                        .chain([name.len()])
                        .collect();
                    assert!(boundaries.contains(&prefix.len()));
                    assert!(boundaries.contains(&(name.len() - suffix.len())));
                } else {
                    assert_eq!(compact.trim_end(), name);
                }
            }
        }
    }

    #[test]
    fn truncation_counter_counts_only_successfully_written_log_records() {
        let text = options(TimeFormat::Epoch, TimeZone::UTC);
        let name = "org.chromium.content.app.SandboxedProcessService0:renderer42";
        let mut e = entry("full message and trailing spaces  \n");
        e.process = Some(Arc::from(name));
        let mut bytes = Vec::new();
        text.write_record(&mut bytes, &e, false).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert_eq!(text.process_names_truncated(), 1);
        assert!(output.ends_with("Tag: full message and trailing spaces  \n"));
        assert!(output.contains("   12    34 I "));
        assert_eq!(e.process_name(), name);
        text.write_record(&mut Vec::new(), &LogEntry::marker("marker"), false)
            .unwrap();
        assert_eq!(text.process_names_truncated(), 1);
        let mut buffer = [];
        let mut unavailable = std::io::Cursor::new(&mut buffer[..]);
        assert!(text.write_record(&mut unavailable, &e, false).is_err());
        assert_eq!(text.process_names_truncated(), 1);
    }

    #[test]
    fn received_time_readable_rfc3339_and_epoch_keep_milliseconds() {
        let e = entry("hi");
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC)
                .time_prefix(&received(e.clone(), Some(1_700_000_000_123)), false),
            "2023-11-14 22:13:20.123+00:00"
        );
        assert_eq!(
            options(TimeFormat::Rfc3339, TimeZone::UTC)
                .time_prefix(&received(e.clone(), Some(1_700_000_000_123)), false),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC)
                .time_prefix(&received(e.clone(), Some(1_700_000_000_123)), false),
            "1700000000.123"
        );
    }

    #[test]
    fn device_nanoseconds_and_extreme_epoch_never_use_floating_point() {
        let e = proto(1_700_000_000, 1);
        assert_eq!(
            options(TimeFormat::Rfc3339, TimeZone::UTC)
                .time_prefix(&received(e.clone(), Some(99)), true),
            "2023-11-14T22:13:20.000000001Z [device]"
        );
        let e = proto(u64::MAX, 999_999_999);
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC).time_prefix(&e, true),
            "18446744073709551615.999999999 [device]"
        );
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC).time_prefix(&e, true),
            "18446744073709551615.999999999 [device; calendar-unavailable]"
        );
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC)
                .time_prefix(&received(e.clone(), Some(u64::MAX)), false),
            "18446744073709551.615"
        );
    }

    #[test]
    fn fixed_offsets_cross_day_and_year_without_changing_instant() {
        let positive = TimeZone::fixed(jiff::tz::Offset::from_seconds(8 * 3600).unwrap());
        let negative =
            TimeZone::fixed(jiff::tz::Offset::from_seconds(-5 * 3600 - 30 * 60).unwrap());
        let time = Some(1_704_067_199_999); // 2023-12-31T23:59:59.999Z
        assert_eq!(
            options(TimeFormat::Readable, positive).time_prefix(&received(entry(""), time), false),
            "2024-01-01 07:59:59.999+08:00"
        );
        assert_eq!(
            options(TimeFormat::Rfc3339, negative).time_prefix(&received(entry(""), time), false),
            "2023-12-31T18:29:59.999-05:30"
        );
    }

    #[test]
    fn second_precision_offsets_do_not_get_silently_rounded_for_rfc3339() {
        let zone = TimeZone::fixed(jiff::tz::Offset::from_seconds(30).unwrap());
        assert_eq!(
            options(TimeFormat::Readable, zone.clone())
                .time_prefix(&received(entry(""), Some(0)), false),
            "1970-01-01 00:00:30.000+00:00:30"
        );
        assert_eq!(
            options(TimeFormat::Rfc3339, zone).time_prefix(&received(entry(""), Some(0)), false),
            "1970-01-01T00:00:00.000Z [received; local-offset-unrepresentable; shown-in-utc]"
        );
    }

    #[test]
    fn missing_time_never_borrows_the_other_clock_or_guesses_a_year() {
        let mut e = proto(1_700_000_000, 2);
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC).time_prefix(&e, false),
            "[received-time-unavailable; device-text=\"10-28 03:43:20.123\"]"
        );
        e.proto = None;
        assert_eq!(
            options(TimeFormat::Readable, TimeZone::UTC)
                .time_prefix(&received(e.clone(), Some(123)), true),
            "[device-time-unavailable; device-text=\"10-28 03:43:20.123\"]"
        );
        e.time = "raw\nambiguous\"time".into();
        assert_eq!(
            options(TimeFormat::Epoch, TimeZone::UTC).time_prefix(&e, true),
            "[device-time-unavailable; device-text=\"raw\\nambiguous\\\"time\"]"
        );
        let invalid = proto(0, 1_000_000_000);
        assert!(
            options(TimeFormat::Epoch, TimeZone::UTC)
                .time_prefix(&invalid, true)
                .contains("device-time-unavailable")
        );
    }

    #[test]
    fn native_receipt_time_uses_provenance_without_guessing_missing_clock() {
        let e = received(entry(""), Some(1234));
        let text = options(TimeFormat::Epoch, TimeZone::UTC);
        assert_eq!(text.time_prefix(&e, false), "1.234");
        let missing = received(e, None);
        assert!(
            text.time_prefix(&missing, false)
                .contains("received-time-unavailable")
        );
        assert!(missing.provenance.is_some());
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
            text.write_record(&mut bytes, &e, false).unwrap();
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
        let e = received(LogEntry::marker("boundary\n"), Some(0));
        let mut bytes = Vec::new();
        text.write_record(&mut bytes, &e, false).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "0.000 [event] boundary\n"
        );
    }

    #[test]
    fn structural_text_ignores_internal_metadata_and_preserves_full_payload() {
        for kind in [
            ciallogcat::model::LogEntryKind::Marker,
            ciallogcat::model::LogEntryKind::Unparsed,
        ] {
            for message in ["", "one\n", "one\r\n\n", "雪\0  "] {
                let mut row = received(entry(message), Some(0));
                row.kind = kind;
                row.process = Some("fake.long.process.identity.that.must.not.display".into());
                let text = options(TimeFormat::Epoch, TimeZone::UTC);
                let mut bytes = Vec::new();
                text.write_record(&mut bytes, &row, false).unwrap();
                let end = if message.ends_with('\n') { "" } else { "\n" };
                assert_eq!(
                    String::from_utf8(bytes).unwrap(),
                    format!("0.000 [{}] {message}{end}", kind.label())
                );
                assert_eq!(text.process_names_truncated(), 0);
                assert_eq!(row.copy_text(), format!("[{}] {message}", kind.label()));
            }
        }
    }

    #[test]
    fn local_unavailability_is_explicit_and_epoch_ignores_timezone() {
        let mut text = options(TimeFormat::Readable, TimeZone::UTC);
        text.local_unavailable = true;
        assert_eq!(
            text.time_prefix(&received(entry(""), Some(0)), false),
            "1970-01-01 00:00:00.000+00:00 [received; local-timezone-unavailable; shown-in-utc]"
        );
        let utc = parse(&[("--time-format", "epoch"), ("--timezone", "utc")]).unwrap();
        let local = parse(&[("--time-format", "epoch"), ("--timezone", "local")]).unwrap();
        assert_eq!(
            utc.time_prefix(&received(entry(""), Some(u64::MAX)), false),
            local.time_prefix(&received(entry(""), Some(u64::MAX)), false)
        );
        assert!(parse(&[("--time-format", "other")]).is_err());
        assert!(parse(&[("--timezone", "+08:00")]).is_err());
    }
}
