use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

use crate::log_store::LogStore;
use crate::model::{Level, LogEntry};

#[derive(Clone, PartialEq)]
pub struct FilterSpec {
    pub package: String,
    /// Case-sensitive literal Tag substrings; independent of the message regex.
    pub tag_include: String,
    pub tag_exclude: String,
    pub min_level: Level,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

#[derive(Clone)]
pub struct FilterRequest {
    pub generation: u64,
    pub log_epoch: u64,
    pub snapshot_base: usize,
    pub entries: Arc<LogStore>,
    pub spec: FilterSpec,
}

pub struct FilterResult {
    pub generation: u64,
    pub log_epoch: u64,
    pub snapshot_base: usize,
    pub processed_len: usize,
    pub matches: Vec<usize>,
    pub elapsed: Duration,
    pub error: Option<String>,
    pub matcher: Option<Regex>,
}

pub type QueryKey = (String, bool, bool);

pub struct HighlightResult {
    pub key: QueryKey,
    pub matcher: Option<Regex>,
}

#[derive(Default)]
struct CompletedFilters {
    filter: Option<FilterResult>,
    highlight: Option<HighlightResult>,
}

pub struct FilterReceiver(Arc<Mutex<CompletedFilters>>);
impl FilterReceiver {
    pub fn take_filter(&self) -> Option<FilterResult> {
        self.0.lock().unwrap().filter.take()
    }
    pub fn take_highlight(&self) -> Option<HighlightResult> {
        self.0.lock().unwrap().highlight.take()
    }
}

#[derive(Default)]
struct PendingFilter {
    request: Option<(u64, FilterRequest)>,
    highlight: Option<(u64, QueryKey)>,
    closed: bool,
}

#[derive(Default)]
struct FilterMailbox {
    pending: Mutex<PendingFilter>,
    ready: Condvar,
    revision: AtomicU64,
    highlight_revision: AtomicU64,
    #[cfg(test)]
    before_scan: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

pub struct FilterSender(Arc<FilterMailbox>);
impl FilterSender {
    pub fn send(&self, request: FilterRequest) {
        let mut pending = self.0.pending.lock().unwrap();
        let revision = self
            .0
            .revision
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        pending.request = Some((revision, request));
        self.0.ready.notify_one();
    }

    pub fn cancel(&self) {
        let mut pending = self.0.pending.lock().unwrap();
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        pending.request = None;
    }

    pub fn send_highlight(&self, key: QueryKey) {
        let mut pending = self.0.pending.lock().unwrap();
        let revision = self
            .0
            .highlight_revision
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        pending.highlight = Some((revision, key));
        self.0.ready.notify_one();
    }
}
impl Drop for FilterSender {
    fn drop(&mut self) {
        let mut pending = self.0.pending.lock().unwrap();
        pending.closed = true;
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        self.0.highlight_revision.fetch_add(1, Ordering::AcqRel);
        pending.request = None;
        pending.highlight = None;
        self.0.ready.notify_one();
    }
}

pub fn spawn_filter_worker(wake: crate::WakeSignal) -> (FilterSender, FilterReceiver) {
    enum Work {
        Filter(u64, FilterRequest),
        Highlight(u64, QueryKey),
    }
    let mailbox = Arc::new(FilterMailbox::default());
    let receiver = Arc::clone(&mailbox);
    // Replacing completed values never blocks the worker when the UI sleeps.
    // Each kind retains only its latest result, including the matcher/error.
    let completed = Arc::new(Mutex::new(CompletedFilters::default()));
    let results = Arc::clone(&completed);
    std::thread::Builder::new()
        .name("log-filter".to_owned())
        .spawn(move || {
            // Draft highlighting and the debounced active filter have distinct
            // requests, but can reuse the same expensive compiled expression.
            let mut cached: Option<(QueryKey, Result<Option<Regex>, String>)> = None;
            loop {
                let work = {
                    let mut pending = receiver.pending.lock().unwrap();
                    while pending.request.is_none()
                        && pending.highlight.is_none()
                        && !pending.closed
                    {
                        pending = receiver.ready.wait(pending).unwrap();
                    }
                    if pending.closed {
                        break;
                    }
                    if let Some((revision, request)) = pending.request.take() {
                        Work::Filter(revision, request)
                    } else {
                        let (revision, key) = pending.highlight.take().unwrap();
                        Work::Highlight(revision, key)
                    }
                };
                let started = Instant::now();
                let current = || match &work {
                    Work::Filter(revision, _) => {
                        receiver.revision.load(Ordering::Acquire) == *revision
                    }
                    Work::Highlight(revision, _) => {
                        receiver.highlight_revision.load(Ordering::Acquire) == *revision
                    }
                };
                if !current() {
                    continue;
                }
                let key = match &work {
                    Work::Filter(_, request) => (
                        request.spec.query.clone(),
                        request.spec.regex,
                        request.spec.case_sensitive,
                    ),
                    Work::Highlight(_, key) => key.clone(),
                };
                if cached.as_ref().is_none_or(|(previous, _)| previous != &key) {
                    let compiled = match &work {
                        Work::Filter(_, request) => compile_matcher(&request.spec),
                        Work::Highlight(_, key) => compile_query(&key.0, key.1, key.2),
                    };
                    cached = Some((key.clone(), compiled));
                }
                if !current() {
                    continue;
                }
                let matcher = cached.as_ref().unwrap().1.clone();
                match &work {
                    Work::Highlight(_, key) => {
                        results.lock().unwrap().highlight = Some(HighlightResult {
                            key: key.clone(),
                            matcher: matcher.ok().flatten(),
                        });
                    }
                    Work::Filter(_, request) => {
                        let (matcher, error) = match matcher {
                            Ok(matcher) => (matcher, None),
                            Err(error) => (None, Some(error)),
                        };
                        #[cfg(test)]
                        if let Some((started, release)) =
                            receiver.before_scan.lock().unwrap().take()
                        {
                            started.send(()).unwrap();
                            release.recv().unwrap();
                        }
                        let matches = if error.is_none() {
                            let Some(matches) =
                                scan_entries(request, matcher.as_ref(), || !current())
                            else {
                                continue;
                            };
                            matches
                        } else {
                            Vec::new()
                        };
                        if !current() {
                            continue;
                        }
                        results.lock().unwrap().filter = Some(FilterResult {
                            generation: request.generation,
                            log_epoch: request.log_epoch,
                            snapshot_base: request.snapshot_base,
                            processed_len: request.entries.len(),
                            matches,
                            elapsed: started.elapsed(),
                            error,
                            matcher,
                        });
                    }
                }
                wake.notify();
            }
        })
        .expect("failed to start filter worker");
    (FilterSender(mailbox), FilterReceiver(completed))
}

// Cooperative cancellation: compile and an individual regex match are not
// preemptible. Check both rows and examined bytes so a few large lines do not
// pin an obsolete snapshot until its entire scan finishes.
const CANCEL_ROWS: usize = 256;
const CANCEL_BYTES: usize = 128 * 1024;

fn scan_entries(
    request: &FilterRequest,
    matcher: Option<&Regex>,
    mut cancelled: impl FnMut() -> bool,
) -> Option<Vec<usize>> {
    let mut matches = Vec::new();
    let mut bytes = 0usize;
    for (index, entry) in request.entries.iter().enumerate() {
        if (index % CANCEL_ROWS == 0 || bytes >= CANCEL_BYTES) && cancelled() {
            return None;
        }
        if index % CANCEL_ROWS == 0 || bytes >= CANCEL_BYTES {
            bytes = 0;
        }
        if entry_matches(entry, &request.spec, matcher) {
            matches.push(index);
        }
        bytes = bytes.saturating_add(entry.payload_bytes());
    }
    (!cancelled()).then_some(matches)
}

pub fn compile_matcher(spec: &FilterSpec) -> Result<Option<Regex>, String> {
    compile_query(&spec.query, spec.regex, spec.case_sensitive)
}

pub fn compile_query(
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> Result<Option<Regex>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    let pattern = if use_regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map(Some)
        .map_err(|error| format!("Invalid regex: {error}"))
}

pub fn entry_matches(entry: &LogEntry, spec: &FilterSpec, matcher: Option<&Regex>) -> bool {
    entry.matches_package(spec.package.trim())
        && (spec.tag_include.is_empty() || entry.tag.contains(&spec.tag_include))
        && (spec.tag_exclude.is_empty() || !entry.tag.contains(&spec.tag_exclude))
        && entry.level >= spec.min_level
        && matcher.is_none_or(|matcher| matcher.is_match(&entry.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(message: &str) -> LogEntry {
        let mut entry = LogEntry::new("", 1, 1, "Tag", Level::Info, message);
        entry.process = Some(Arc::from("com.example.app:worker"));
        entry
    }

    #[test]
    fn plain_text_is_not_treated_as_regex() {
        let spec = FilterSpec {
            package: String::new(),
            tag_include: String::new(),
            tag_exclude: String::new(),
            min_level: Level::Verbose,
            query: ".*".to_owned(),
            regex: false,
            case_sensitive: false,
        };
        let matcher = compile_matcher(&spec).unwrap().unwrap();
        assert!(matcher.is_match("literal .* value"));
        assert!(!matcher.is_match("anything"));
    }

    #[test]
    fn tag_literals_are_case_sensitive_and_exclusion_wins_independently_of_message_regex() {
        let mut spec = FilterSpec {
            package: String::new(),
            tag_include: "Net.*".into(),
            tag_exclude: "Noise".into(),
            min_level: Level::Verbose,
            query: "request|response".into(),
            regex: true,
            case_sensitive: false,
        };
        let matcher = compile_matcher(&spec).unwrap();
        for (tag, expected) in [
            ("Net.*Client", true),
            ("Net.*Noise", false),
            ("net.*Client", false),
            ("Network", false),
        ] {
            let row = LogEntry::new("", 1, 1, tag, Level::Info, "REQUEST");
            assert_eq!(
                entry_matches(&row, &spec, matcher.as_ref()),
                expected,
                "{tag}"
            );
        }
        spec.tag_include.clear();
        spec.tag_exclude.clear();
        assert!(entry_matches(&entry("REQUEST"), &spec, matcher.as_ref()));
    }

    #[test]
    fn package_and_regex_are_combined() {
        let spec = FilterSpec {
            package: "com.example.app".to_owned(),
            tag_include: String::new(),
            tag_exclude: String::new(),
            min_level: Level::Debug,
            query: "timeout|failed".to_owned(),
            regex: true,
            case_sensitive: false,
        };
        let matcher = compile_matcher(&spec).unwrap().unwrap();
        assert!(entry_matches(
            &entry("Request FAILED"),
            &spec,
            Some(&matcher)
        ));
        assert!(!entry_matches(
            &entry("Request complete"),
            &spec,
            Some(&matcher)
        ));
    }
    #[test]
    fn obsolete_scans_cancel_at_row_and_byte_checkpoints() {
        for payload in [1, CANCEL_BYTES] {
            let mut entries = LogStore::default();
            let rows = if payload == 1 { CANCEL_ROWS * 3 } else { 3 };
            for _ in 0..rows {
                entries.push(LogEntry::marker("x".repeat(payload)));
            }
            let request = FilterRequest {
                generation: 1,
                log_epoch: 0,
                snapshot_base: 0,
                entries: Arc::new(entries),
                spec: FilterSpec {
                    package: String::new(),
                    tag_include: String::new(),
                    tag_exclude: String::new(),
                    min_level: Level::Verbose,
                    query: String::new(),
                    regex: false,
                    case_sensitive: false,
                },
            };
            let mut calls = 0;
            let result = scan_entries(&request, None, || {
                calls += 1;
                calls == 2
            });
            assert!(result.is_none());
            assert_eq!(calls, 2);
            assert_eq!(
                scan_entries(&request, None, || false).unwrap().len(),
                request.entries.len()
            );
        }
    }
    #[test]
    fn superseding_and_closing_worker_release_obsolete_snapshot_ownership() {
        fn request(generation: u64, entries: Arc<LogStore>) -> FilterRequest {
            FilterRequest {
                generation,
                log_epoch: 1,
                snapshot_base: 0,
                entries,
                spec: FilterSpec {
                    package: String::new(),
                    tag_include: String::new(),
                    tag_exclude: String::new(),
                    min_level: Level::Verbose,
                    query: "missing|needle".to_owned(),
                    regex: true,
                    case_sensitive: false,
                },
            }
        }
        for close in [false, true] {
            let (sender, receiver) = spawn_filter_worker(crate::WakeSignal::default());
            let mut entries = LogStore::default();
            let message: Arc<str> = Arc::from("x".repeat(4096));
            for _ in 0..20_000 {
                entries.push(LogEntry::marker(Arc::clone(&message)));
            }
            let entries = Arc::new(entries);
            let obsolete = Arc::downgrade(&entries);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            *sender.0.before_scan.lock().unwrap() = Some((started_tx, release_rx));
            sender.send(request(1, entries));
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("worker owns active snapshot");
            assert!(obsolete.upgrade().is_some());
            if !close {
                sender.send(request(2, Arc::new(LogStore::default())));
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            if close {
                drop(sender);
            }
            release_tx.send(()).unwrap();
            while obsolete.upgrade().is_some() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(
                obsolete.upgrade().is_none(),
                "obsolete snapshot ownership released"
            );
            if !close {
                loop {
                    if let Some(result) = receiver.take_filter()
                        && result.generation == 2
                    {
                        assert_eq!(result.log_epoch, 1);
                        assert!(result.matches.is_empty());
                        break;
                    }
                    assert!(Instant::now() < deadline, "latest result published");
                    std::thread::yield_now();
                }
            }
        }
    }
}
