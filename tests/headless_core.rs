//! Public consumer coverage: these tests compile and run without the GUI feature.
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use ciallogcat::WakeSignal;
use ciallogcat::adb::{BackendEvent, parse_logcat_line, spawn_device_query};
use ciallogcat::filter::{
    FilterReceiver, FilterRequest, FilterSpec, compile_matcher, entry_matches, spawn_filter_worker,
};
use ciallogcat::log_store::LogStore;
use ciallogcat::model::{Level, LogEntry, LogEntryKind};

const WAIT: Duration = Duration::from_secs(5);

fn spec(package: &str, level: Level, query: &str, regex: bool, case: bool) -> FilterSpec {
    FilterSpec {
        tag_include: String::new(),
        tag_exclude: String::new(),
        package: package.into(),
        min_level: level,
        query: query.into(),
        regex,
        case_sensitive: case,
    }
}

fn request(generation: u64, entries: Arc<LogStore>, spec: FilterSpec) -> FilterRequest {
    FilterRequest {
        generation,
        log_epoch: 42,
        snapshot_base: 17,
        entries,
        spec,
    }
}

#[test]
fn public_parser_preserves_threadtime_and_fallback_contract() {
    for (line, time, pid, tid, level, tag, message, kind) in [
        (
            "10-16 09:00:01.1 123 456 I 标签: café 雪 🚀",
            "10-16 09:00:01.1",
            123,
            456,
            Level::Info,
            "标签",
            "café 雪 🚀",
            LogEntryKind::Log,
        ),
        (
            "2026-10-16 09:00:01.123456 7 8 A Fatal: ",
            "2026-10-16 09:00:01.123456",
            7,
            8,
            Level::Fatal,
            "Fatal",
            "",
            LogEntryKind::Log,
        ),
        (
            "--------- beginning of main",
            "",
            0,
            0,
            Level::Info,
            "logcat",
            "--------- beginning of main",
            LogEntryKind::Marker,
        ),
        (
            " unexpected text ",
            "",
            0,
            0,
            Level::Info,
            "logcat",
            " unexpected text ",
            LogEntryKind::Unparsed,
        ),
    ] {
        let entry = parse_logcat_line(line).unwrap();
        assert_eq!(entry.time.as_ref(), time);
        assert_eq!((entry.pid, entry.tid, entry.level), (pid, tid, level));
        assert_eq!(entry.tag.as_ref(), tag);
        assert_eq!(entry.message.as_ref(), message);
        assert!(entry.process.is_none());
        assert_eq!(entry.kind, kind);
    }
    assert!(parse_logcat_line(" \t").is_none());
    assert_eq!(
        parse_logcat_line("10-16 09:00:01.123 0 0 I Kernel: parsed pid zero")
            .unwrap()
            .kind,
        LogEntryKind::Log
    );
    for line in [
        "10-16 09:00:01.123 1 2 S Tag: silent",
        "10-16 09:00:01.123 4294967296 2 I Tag: overflowing pid",
        "10-16 09:00:01.123 1 4294967296 I Tag: overflowing tid",
    ] {
        let entry = parse_logcat_line(line).unwrap();
        assert_eq!(entry.kind, LogEntryKind::Unparsed, "{line:?}");
        assert_eq!(entry.message.as_ref(), line);
    }
}

#[test]
fn structural_markers_are_not_unknown_logs_and_keep_other_filter_conditions() {
    let marker = LogEntry::marker("--------- beginning of crash");
    let unparsed = LogEntry::unparsed("unrecognized device output");
    let log = LogEntry::new("", 42, 42, "Tag", Level::Info, "unknown process");
    for (filter, marker_matches) in [
        (spec("com.example.app", Level::Info, "", false, false), true),
        (
            spec("com.example.app", Level::Warn, "", false, false),
            false,
        ),
        (
            spec("com.example.app", Level::Info, "crash", false, false),
            true,
        ),
        (
            spec("com.example.app", Level::Info, "missing", false, false),
            false,
        ),
    ] {
        let matcher = compile_matcher(&filter).unwrap();
        assert_eq!(
            entry_matches(&marker, &filter, matcher.as_ref()),
            marker_matches
        );
        assert!(!entry_matches(&unparsed, &filter, matcher.as_ref()));
        assert!(!entry_matches(&log, &filter, matcher.as_ref()));
    }
    let mut store = LogStore::default();
    store.push(marker);
    store.push(unparsed);
    store.push(log);
    assert_eq!(store.unresolved_count(), 1);
}

#[test]
fn public_filter_matches_expected_corpus_with_noop_wake() {
    let mut store = LogStore::default();
    for (level, process, tag, message) in [
        (
            Level::Info,
            Some("com.example.app"),
            "Tag",
            "literal .* café",
        ),
        (
            Level::Warn,
            Some("com.example.app:worker"),
            "Tag",
            "Request FAILED",
        ),
        (
            Level::Error,
            Some("com.example.app2"),
            "Tag",
            "request failed",
        ),
        (Level::Debug, None, "Tag", "Request failed"),
        (Level::Fatal, Some("com.example.app"), "needle", "complete"),
        (
            Level::Warn,
            Some("com.example.app"),
            "Tag",
            "request failed",
        ),
    ] {
        let mut entry = LogEntry::new("", 1, 2, tag, level, message);
        entry.process = process.map(Arc::from);
        store.push(entry);
    }
    let entries = Arc::new(store.clone());
    store.drain_front(1);
    store.set_process(0, Arc::from("unrelated.app"));
    assert_eq!(entries.len(), 6, "filter snapshot stays immutable");
    let (sender, receiver) = spawn_filter_worker(WakeSignal::default());
    let cases = [
        (spec("", Level::Verbose, ".*", false, false), vec![0]),
        (
            spec("", Level::Verbose, ".*", true, false),
            vec![0, 1, 2, 3, 4, 5],
        ),
        (
            spec(
                " com.example.app ",
                Level::Warn,
                "request failed",
                true,
                false,
            ),
            vec![1, 5],
        ),
        (
            spec(
                "com.example.app",
                Level::Warn,
                "request failed",
                false,
                true,
            ),
            vec![5],
        ),
        (
            spec("com.example.app", Level::Error, "", false, false),
            vec![4],
        ),
        (
            spec("", Level::Verbose, "Request failed", false, false),
            vec![1, 2, 3, 5],
        ),
        (
            spec("", Level::Verbose, "needle|com.example.app", true, false),
            vec![],
        ),
    ];
    for (generation, (spec, expected)) in cases.into_iter().enumerate() {
        let matcher = compile_matcher(&spec).unwrap();
        let synchronous: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry_matches(entry, &spec, matcher.as_ref()))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(synchronous, expected, "case {generation}");
        sender.send(request(generation as u64, Arc::clone(&entries), spec));
        let deadline = Instant::now() + WAIT;
        let result = loop {
            if let Some(result) = receiver.take_filter() {
                break result;
            }
            assert!(Instant::now() < deadline, "filter completion missing");
            std::thread::yield_now();
        };
        assert_eq!(result.generation, generation as u64);
        assert_eq!(
            (result.log_epoch, result.snapshot_base, result.processed_len),
            (42, 17, 6)
        );
        assert_eq!(result.matches, expected);
        assert!(result.error.is_none());
    }
}

#[test]
fn callback_can_take_published_filter_and_highlight_results() {
    let slot: Arc<Mutex<Option<FilterReceiver>>> = Arc::new(Mutex::new(None));
    let callback_slot = Arc::clone(&slot);
    let (completed_tx, completed_rx) = mpsc::channel();
    let wake = WakeSignal::new(move || {
        // Taking results here also verifies the internal result lock is released
        // before invoking user code. The slot is installed before any request.
        let slot = callback_slot.lock().unwrap();
        let receiver = slot.as_ref().unwrap();
        completed_tx
            .send((receiver.take_filter(), receiver.take_highlight()))
            .unwrap();
    });
    let (sender, receiver) = spawn_filter_worker(wake.clone());
    *slot.lock().unwrap() = Some(receiver);
    let invalid = spec("", Level::Verbose, "[", true, false);
    assert!(compile_matcher(&invalid).is_err());
    sender.send(request(1, Arc::new(LogStore::default()), invalid));
    let (filter, highlight) = completed_rx.recv_timeout(WAIT).unwrap();
    assert!(highlight.is_none());
    let filter = filter.expect("result must be published before wake");
    assert_eq!(filter.generation, 1);
    assert!(
        filter
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("Invalid regex:"))
    );
    assert!(filter.matches.is_empty());

    let mut entries = LogStore::default();
    entries.push(LogEntry::marker("needle"));
    sender.send(request(
        2,
        Arc::new(entries),
        spec("", Level::Info, "needle", false, false),
    ));
    let (filter, _) = completed_rx.recv_timeout(WAIT).unwrap();
    let filter = filter.expect("worker must recover after invalid regex");
    assert_eq!(filter.generation, 2);
    assert_eq!(filter.matches, [0]);
    assert!(filter.error.is_none());

    let key = ("café".to_owned(), false, false);
    sender.send_highlight(key.clone());
    let (filter, highlight) = completed_rx.recv_timeout(WAIT).unwrap();
    assert!(filter.is_none());
    let highlight = highlight.expect("highlight must be published before wake");
    assert_eq!(highlight.key, key);
    assert!(highlight.matcher.unwrap().is_match("CAFÉ"));
}

#[test]
fn backend_callback_can_receive_already_published_event() {
    let (event_tx, event_rx) = mpsc::sync_channel(1);
    let event_rx = Mutex::new(event_rx);
    let (observed_tx, observed_rx) = mpsc::channel();
    let wake = WakeSignal::new(move || {
        let published_error = matches!(
            event_rx.lock().unwrap().try_recv(),
            Ok(BackendEvent::Devices(Err(_)))
        );
        observed_tx.send(published_error).unwrap();
    });
    // A nonexistent child path is deterministic on every platform and exercises
    // the production error event without ADB, fixture processes, or environment changes.
    let missing = std::env::current_exe()
        .unwrap()
        .join("not-an-adb-executable");
    assert!(!missing.exists());
    spawn_device_query(missing, event_tx, wake.clone());
    assert!(
        observed_rx.recv_timeout(WAIT).unwrap(),
        "event must precede wake"
    );
}
