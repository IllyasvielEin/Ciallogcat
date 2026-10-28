//! Query predicates share the desktop message/Tag/package semantics. Time ranges
//! are UTC instants, never inferred from yearless device display strings.
use std::collections::BTreeMap;

use ciallogcat::filter::{FilterSpec, compile_matcher, entry_matches};
use ciallogcat::model::{Level, LogEntry, LogEntryKind};
use regex::Regex;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{Result, error, usage};

pub(super) struct QueryFilter {
    spec: FilterSpec,
    matcher: Option<Regex>,
    pid: Option<u32>,
    tid: Option<u32>,
    since: Option<i128>,
    until: Option<i128>,
    device_time: bool,
    since_input: Option<String>,
    until_input: Option<String>,
    fields_required: bool,
}

impl QueryFilter {
    /// The caller binds this anchor to its snapshot cursor so relative bounds do
    /// not drift while paging or streaming a long-running query.
    pub(super) fn parse(flags: &mut BTreeMap<String, String>, anchor_ms: u64) -> Result<Self> {
        let query = string(flags, "--query")?;
        let regex = flags.remove("--regex").is_some();
        let insensitive = flags.remove("--ignore-case").is_some();
        let sensitive = flags.remove("--case-sensitive").is_some();
        if insensitive && sensitive {
            return Err(usage(
                "--ignore-case and --case-sensitive are mutually exclusive",
            ));
        }
        let tag = flags.remove("--tag");
        let tag_include = flags.remove("--tag-include");
        if tag.is_some() && tag_include.is_some() {
            return Err(usage(
                "--tag and --tag-include are aliases; provide only one",
            ));
        }
        let tag_include = bounded(tag.or(tag_include).unwrap_or_default(), "--tag")?;
        let tag_exclude = string(flags, "--tag-exclude")?;
        let package = string(flags, "--package")?;
        let level = flags.remove("--min-level");
        let min_level = match level
            .as_deref()
            .unwrap_or("V")
            .to_ascii_uppercase()
            .as_str()
        {
            "V" | "VERBOSE" => Level::Verbose,
            "D" | "DEBUG" => Level::Debug,
            "I" | "INFO" => Level::Info,
            "W" | "WARN" | "WARNING" => Level::Warn,
            "E" | "ERROR" => Level::Error,
            "F" | "FATAL" | "A" | "ASSERT" => Level::Fatal,
            _ => {
                return Err(usage(
                    "--min-level must be V, D, I, W, E or F (or full name)",
                ));
            }
        };
        let pid = id(flags, "--pid")?;
        let tid = id(flags, "--tid")?;
        let since_input = flags.remove("--since");
        let until_input = flags.remove("--until");
        let since = since_input
            .as_deref()
            .map(|v| parse_time(v, anchor_ms))
            .transpose()?;
        let until = until_input
            .as_deref()
            .map(|v| parse_time(v, anchor_ms))
            .transpose()?;
        if since.zip(until).is_some_and(|(start, end)| start >= end) {
            return Err(usage("--since must be earlier than --until (exclusive)"));
        }
        let device_time = match flags
            .remove("--time-basis")
            .as_deref()
            .unwrap_or("received")
        {
            "received" => false,
            "device" => true,
            _ => return Err(usage("--time-basis must be received or device")),
        };
        let fields_required = !tag_include.is_empty()
            || !tag_exclude.is_empty()
            || !package.trim().is_empty()
            || pid.is_some()
            || tid.is_some()
            || level.is_some();
        let spec = FilterSpec {
            package,
            tag_include,
            tag_exclude,
            min_level,
            query,
            regex,
            case_sensitive: !insensitive,
        };
        let matcher = compile_matcher(&spec).map_err(usage)?;
        Ok(Self {
            spec,
            matcher,
            pid,
            tid,
            since,
            until,
            device_time,
            fields_required,
            since_input,
            until_input,
        })
    }

    pub(super) fn device_time(&self) -> bool {
        self.device_time
    }

    pub(super) fn has_time_filter(&self) -> bool {
        self.since.is_some() || self.until.is_some()
    }

    pub(super) fn fingerprint(&self) -> String {
        // Keep pre-upgrade literal-only cursors valid. Extended filters use a
        // versioned canonical encoding so every effective predicate is bound.
        let bytes = if !self.spec.regex
            && self.spec.case_sensitive
            && !self.fields_required
            && self.since.is_none()
            && self.until.is_none()
        {
            self.spec.query.as_bytes().to_vec()
        } else {
            let canonical = json!({"query_filter_version":1,"query":self.spec.query,
                "regex":self.spec.regex,"case_sensitive":self.spec.case_sensitive,
                "tag_include":self.spec.tag_include,"tag_exclude":self.spec.tag_exclude,
                "package":self.spec.package.trim(),"min_level":self.spec.min_level.short(),
                "pid":self.pid,"tid":self.tid,"since_ns":self.since,"until_ns":self.until,
                "since_input":self.since_input,"until_input":self.until_input,
                "device_time":self.device_time && (self.since.is_some() || self.until.is_some()),
                "fields_required":self.fields_required})
            .to_string();
            // NUL cannot occur in an argv query: separate extended hashes from
            // legacy arbitrary literal queries.
            let mut bytes = b"\0ciallogcat-query-filter-v1\0".to_vec();
            bytes.extend_from_slice(canonical.as_bytes());
            bytes
        };
        format!("{:x}", Sha256::digest(bytes))
    }

    pub(super) fn matches(
        &self,
        entry: &LogEntry,
        legacy_received_unix_ms: Option<u64>,
    ) -> Result<bool> {
        if (self.fields_required && entry.kind != LogEntryKind::Log)
            || self.pid.is_some_and(|pid| entry.pid != pid)
            || self.tid.is_some_and(|tid| entry.tid != tid)
            || !entry_matches(entry, &self.spec, self.matcher.as_ref())
        {
            return Ok(false);
        }
        if self.has_time_filter() {
            let time = if self.device_time {
                entry.proto.as_ref().map(|p| i128::from(p.time_sec) * 1_000_000_000 + i128::from(p.time_nsec))
            } else {
                entry.provenance.and_then(|p| p.observed_unix_ms).or(legacy_received_unix_ms).map(|ms| i128::from(ms) * 1_000_000)
            }.ok_or_else(|| error("time_unavailable", if self.device_time {
                "device time filtering requires an exact protobuf epoch on every otherwise matching record; yearless/text timestamps are not inferred"
            } else {
                "received time filtering requires observed_unix_ms on every otherwise matching record; older legacy records may not contain it"
            }, 3))?;
            if self.since.is_some_and(|start| time < start)
                || self.until.is_some_and(|end| time >= end)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn bounded(value: String, flag: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(usage(format!("{flag} cannot contain NUL")));
    }
    if value.len() > 4096 {
        return Err(usage(format!("{flag} exceeds 4096 UTF-8 bytes")));
    }
    Ok(value)
}
fn string(flags: &mut BTreeMap<String, String>, flag: &str) -> Result<String> {
    bounded(flags.remove(flag).unwrap_or_default(), flag)
}
fn id(flags: &mut BTreeMap<String, String>, flag: &str) -> Result<Option<u32>> {
    flags
        .remove(flag)
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(usage(format!(
                    "{flag} requires an unsigned decimal process/thread ID"
                )));
            }
            value
                .parse()
                .map_err(|_| usage(format!("{flag} exceeds u32 range")))
        })
        .transpose()
}

/// Inclusive since / exclusive until, represented as signed UTC nanoseconds.
/// RFC3339 leap seconds and unknown -00:00 offsets cannot be resolved reliably
/// by this clock model and are rejected rather than rounded or guessed.
fn parse_time(value: &str, anchor_ms: u64) -> Result<i128> {
    let invalid = || {
        usage(
            "time must be RFC3339 with a known timezone (no leap seconds), or an integer relative duration such as 30s, 5m, 2h, 1d",
        )
    };
    if let Some(unit) = value.as_bytes().last().copied()
        && matches!(unit, b's' | b'm' | b'h' | b'd')
    {
        let digits = &value[..value.len() - 1];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let amount: i128 = digits.parse().map_err(|_| invalid())?;
        let seconds = match unit {
            b's' => 1,
            b'm' => 60,
            b'h' => 3600,
            _ => 86400,
        };
        let duration = amount
            .checked_mul(seconds * 1_000_000_000)
            .ok_or_else(invalid)?;
        return (i128::from(anchor_ms) * 1_000_000)
            .checked_sub(duration)
            .ok_or_else(invalid);
    }
    let b = value.as_bytes();
    if b.len() < 20
        || !b.is_ascii()
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return Err(invalid());
    }
    let number = |start: usize, end: usize| -> Result<i64> {
        let bytes = &b[start..end];
        if !bytes.iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        value[start..end].parse().map_err(|_| invalid())
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid());
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days).contains(&day) {
        return Err(invalid());
    }
    let mut index = 19;
    let mut nanos = 0i128;
    if b.get(index) == Some(&b'.') {
        index += 1;
        let begin = index;
        while b.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let length = index - begin;
        if !(1..=9).contains(&length) {
            return Err(invalid());
        }
        nanos = value[begin..index].parse::<i128>().map_err(|_| invalid())?
            * 10i128.pow(9 - length as u32);
    }
    let offset = match &b[index..] {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2]
            if [h1, h2, m1, m2].iter().all(|v| v.is_ascii_digit()) =>
        {
            let hours = i64::from((h1 - b'0') * 10 + h2 - b'0');
            let minutes = i64::from((m1 - b'0') * 10 + m2 - b'0');
            if hours > 23 || minutes > 59 || (*sign == b'-' && hours == 0 && minutes == 0) {
                return Err(invalid());
            }
            (hours * 3600 + minutes * 60) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return Err(invalid()),
    };
    // Gregorian civil date to days since Unix epoch (Howard Hinnant's era decomposition).
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Ok(
        i128::from(days * 86400 + hour * 3600 + minute * 60 + second - offset) * 1_000_000_000
            + nanos,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciallogcat::session::{RecordIdentity, SessionId};

    fn parse(flags: &[(&str, &str)]) -> Result<QueryFilter> {
        QueryFilter::parse(
            &mut flags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            1_700_000_000_000,
        )
    }
    fn entry() -> LogEntry {
        let mut e = LogEntry::new(
            "01-01 00:00:00.000",
            42,
            43,
            "NetWorker",
            Level::Warn,
            "Hello World",
        );
        e.process = Some("com.example.app:worker".into());
        e.provenance = Some(RecordIdentity {
            session_id: SessionId::new(),
            seq: 1,
            segment: 1,
            attempt: 1,
            observed_unix_ms: Some(1_700_000_000_000),
        });
        e
    }

    #[test]
    fn preserves_legacy_literal_case_sensitive_hash_and_matches() {
        let f = parse(&[("--query", "Hello")]).unwrap();
        assert_eq!(f.fingerprint(), format!("{:x}", Sha256::digest(b"Hello")));
        assert!(f.matches(&entry(), None).unwrap());
        assert!(
            !parse(&[("--query", "hello")])
                .unwrap()
                .matches(&entry(), None)
                .unwrap()
        );
        assert!(
            !parse(&[("--query", ".*")])
                .unwrap()
                .matches(&entry(), None)
                .unwrap()
        );
        assert!(!f.has_time_filter());
    }
    #[test]
    fn shared_gui_regex_case_tag_and_package_semantics() {
        let f = parse(&[
            ("--query", "^hello w.*d$"),
            ("--regex", ""),
            ("--ignore-case", ""),
            ("--tag", "Net"),
            ("--tag-exclude", "Other"),
            ("--package", "com.example.app"),
            ("--min-level", "warn"),
            ("--pid", "42"),
            ("--tid", "43"),
        ])
        .unwrap();
        assert!(f.matches(&entry(), None).unwrap());
        for flags in [
            vec![("--tag", "net")],
            vec![("--tag-exclude", "Worker")],
            vec![("--pid", "43")],
            vec![("--tid", "42")],
            vec![("--min-level", "E")],
            vec![("--package", "com.other")],
        ] {
            assert!(!parse(&flags).unwrap().matches(&entry(), None).unwrap());
        }
        assert!(parse(&[("--query", "["), ("--regex", "")]).is_err());
        assert!(parse(&[("--ignore-case", ""), ("--case-sensitive", "")]).is_err());
        assert!(parse(&[("--tag", "a"), ("--tag-include", "b")]).is_err());
        assert!(parse(&[("--pid", "4294967296")]).is_err());
        assert!(parse(&[("--pid", "+1")]).is_err());
        assert!(parse(&[("--min-level", "X")]).is_err());
    }
    #[test]
    fn structural_rows_do_not_match_placeholder_pid_or_level() {
        let e = LogEntry::marker("banner");
        assert!(parse(&[]).unwrap().matches(&e, None).unwrap());
        assert!(!parse(&[("--pid", "0")]).unwrap().matches(&e, None).unwrap());
        assert!(
            !parse(&[("--min-level", "V")])
                .unwrap()
                .matches(&e, None)
                .unwrap()
        );
    }
    #[test]
    fn absolute_time_offset_fraction_day_and_year_crossings() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z", 0).unwrap(), 0);
        assert_eq!(parse_time("1970-01-01T01:00:00+01:00", 0).unwrap(), 0);
        assert_eq!(parse_time("1969-12-31T19:00:00-05:00", 0).unwrap(), 0);
        assert_eq!(parse_time("1969-12-31T23:59:59.999999999Z", 0).unwrap(), -1);
        assert_eq!(
            parse_time("1970-01-01T00:00:00.123456789z", 0).unwrap(),
            123456789
        );
        assert_eq!(
            parse_time("2024-03-01T00:00:00Z", 0).unwrap()
                - parse_time("2024-02-28T00:00:00Z", 0).unwrap(),
            2 * 86400 * 1_000_000_000
        );
        assert_eq!(
            parse_time("2025-01-01T00:00:00+02:00", 0).unwrap(),
            parse_time("2024-12-31T22:00:00Z", 0).unwrap()
        );
        assert_eq!(
            parse_time("2000-03-01T00:00:00Z", 0).unwrap()
                - parse_time("2000-02-28T00:00:00Z", 0).unwrap(),
            2 * 86400 * 1_000_000_000
        );
        assert!(parse_time("1900-02-29T00:00:00Z", 0).is_err());
    }
    #[test]
    fn rejects_ambiguous_and_invalid_times_without_guessing() {
        for value in [
            "01-01 12:00:00.000",
            "2024-01-01T12:00:00",
            "2024-01-01T12:00:00-00:00",
            "2024-01-01T12:00:60Z",
            "2023-02-29T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00.1234567890Z",
            "2024-01-01T00:00:00.Z",
            "",
            "s",
            "-1s",
            "1.5h",
            "999999999999999999999999999999999999999999999d",
            "2024-01-01T00:00:00Ztrailing",
        ] {
            assert!(parse_time(value, 0).is_err(), "accepted {value}");
        }
    }
    #[test]
    fn relative_anchor_and_inclusive_exclusive_boundaries() {
        assert_eq!(parse_time("30s", 60_000).unwrap(), 30_000_000_000);
        assert_eq!(parse_time("5m", 600_000).unwrap(), 300_000_000_000);
        assert_eq!(parse_time("2h", 7_200_000).unwrap(), 0);
        assert_eq!(parse_time("1d", 86_400_000).unwrap(), 0);
        let e = entry();
        assert!(
            parse(&[("--since", "0s")])
                .unwrap()
                .matches(&e, None)
                .unwrap()
        );
        assert!(
            !parse(&[("--until", "0s")])
                .unwrap()
                .matches(&e, None)
                .unwrap()
        );
        assert!(parse(&[("--since", "1s"), ("--until", "0s")]).is_ok());
        assert!(parse(&[("--since", "0s"), ("--until", "1s")]).is_err());
        assert!(parse(&[("--since", "0s"), ("--until", "0s")]).is_err());
        let flags = BTreeMap::from([("--since".into(), "1h".into())]);
        let first = QueryFilter::parse(&mut flags.clone(), 10000).unwrap();
        let same = QueryFilter::parse(&mut flags.clone(), 10000).unwrap();
        let later = QueryFilter::parse(&mut flags.clone(), 20000).unwrap();
        assert_eq!(first.fingerprint(), same.fingerprint());
        assert_ne!(first.fingerprint(), later.fingerprint());
    }
    #[test]
    fn missing_time_is_explicit_and_unrelated_nonmatches_are_skipped() {
        let mut e = entry();
        e.provenance = None;
        assert_eq!(
            parse(&[("--since", "1h")])
                .unwrap()
                .matches(&e, None)
                .unwrap_err()
                .code,
            "time_unavailable"
        );
        assert_eq!(
            parse(&[("--since", "1h"), ("--time-basis", "device")])
                .unwrap()
                .matches(&e, None)
                .unwrap_err()
                .code,
            "time_unavailable"
        );
        assert!(
            !parse(&[("--since", "1h"), ("--query", "absent")])
                .unwrap()
                .matches(&e, None)
                .unwrap()
        );
    }
    #[test]
    fn fingerprints_bind_all_effective_filters() {
        let baseline = parse(&[]).unwrap().fingerprint();
        for flags in [
            vec![("--regex", "")],
            vec![("--ignore-case", "")],
            vec![("--tag", "Net")],
            vec![("--tag-exclude", "Net")],
            vec![("--package", "com.example")],
            vec![("--pid", "42")],
            vec![("--tid", "43")],
            vec![("--min-level", "V")],
            vec![("--since", "1h")],
        ] {
            assert_ne!(parse(&flags).unwrap().fingerprint(), baseline);
        }
    }
    #[test]
    fn device_epoch_has_nanosecond_precision_not_host_clock() {
        let mut e = entry();
        e.proto = Some(std::sync::Arc::new(ciallogcat::model::ProtoEvidence {
            time_sec: 0,
            time_nsec: 123456789,
            uid: 1,
            pid: 42,
            tid: 43,
            priority: 5,
            raw_payload: Vec::new(),
            tag_bytes: Vec::new(),
            message_bytes: Vec::new(),
            process_name: None,
        }));
        assert!(
            parse(&[
                ("--time-basis", "device"),
                ("--since", "1970-01-01T00:00:00.123456789Z"),
                ("--until", "1970-01-01T00:00:00.123456790Z")
            ])
            .unwrap()
            .matches(&e, None)
            .unwrap()
        );
        assert!(
            !parse(&[
                ("--time-basis", "device"),
                ("--until", "1970-01-01T00:00:00.123456789Z")
            ])
            .unwrap()
            .matches(&e, None)
            .unwrap()
        );
        assert!(
            !parse(&[("--until", "1970-01-01T00:00:00.123456790Z")])
                .unwrap()
                .matches(&e, None)
                .unwrap()
        );
    }
    #[test]
    fn optional_legacy_received_time_does_not_require_fabricated_identity() {
        let mut e = entry();
        e.provenance = None;
        let filter = parse(&[
            ("--since", "1970-01-01T00:00:00.123Z"),
            ("--until", "1970-01-01T00:00:00.124Z"),
        ])
        .unwrap();
        assert!(filter.matches(&e, Some(123)).unwrap());
        assert!(!filter.matches(&e, Some(122)).unwrap());
        assert!(!filter.matches(&e, Some(124)).unwrap());
        assert_eq!(
            filter.matches(&e, None).unwrap_err().code,
            "time_unavailable"
        );
        assert_eq!(
            parse(&[("--since", "0s"), ("--time-basis", "device")])
                .unwrap()
                .matches(&e, Some(123))
                .unwrap_err()
                .code,
            "time_unavailable"
        );
        assert!(e.provenance.is_none());
    }
}
