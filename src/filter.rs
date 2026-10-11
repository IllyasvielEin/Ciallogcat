use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

use crate::log_store::LogStore;
use crate::model::{Level, LogEntry};

#[derive(Clone)]
pub struct FilterSpec {
    pub package: String,
    pub min_level: Level,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

#[derive(Clone)]
pub struct FilterRequest {
    pub generation: u64,
    pub entries: Arc<LogStore>,
    pub spec: FilterSpec,
}

pub struct FilterResult {
    pub generation: u64,
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
    request: Option<FilterRequest>,
    highlight: Option<QueryKey>,
    closed: bool,
}

#[derive(Default)]
struct FilterMailbox {
    pending: Mutex<PendingFilter>,
    ready: Condvar,
}

pub struct FilterSender(Arc<FilterMailbox>);
impl FilterSender {
    pub fn send(&self, request: FilterRequest) {
        self.0.pending.lock().unwrap().request = Some(request);
        self.0.ready.notify_one();
    }

    pub fn send_highlight(&self, key: QueryKey) {
        self.0.pending.lock().unwrap().highlight = Some(key);
        self.0.ready.notify_one();
    }
}
impl Drop for FilterSender {
    fn drop(&mut self) {
        let mut pending = self.0.pending.lock().unwrap();
        pending.closed = true;
        pending.request = None;
        pending.highlight = None;
        self.0.ready.notify_one();
    }
}

pub fn spawn_filter_worker(ctx: eframe::egui::Context) -> (FilterSender, FilterReceiver) {
    enum Work {
        Filter(FilterRequest),
        Highlight(QueryKey),
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
                    if let Some(request) = pending.request.take() {
                        Work::Filter(request)
                    } else {
                        Work::Highlight(pending.highlight.take().unwrap())
                    }
                };
                let started = Instant::now();
                let key = match &work {
                    Work::Filter(request) => (
                        request.spec.query.clone(),
                        request.spec.regex,
                        request.spec.case_sensitive,
                    ),
                    Work::Highlight(key) => key.clone(),
                };
                if cached.as_ref().is_none_or(|(previous, _)| previous != &key) {
                    let compiled = match &work {
                        Work::Filter(request) => compile_matcher(&request.spec),
                        Work::Highlight(key) => compile_query(&key.0, key.1, key.2),
                    };
                    cached = Some((key.clone(), compiled));
                }
                let matcher = cached.as_ref().unwrap().1.clone();
                match work {
                    Work::Highlight(key) => {
                        results.lock().unwrap().highlight = Some(HighlightResult {
                            key,
                            matcher: matcher.ok().flatten(),
                        });
                    }
                    Work::Filter(request) => {
                        let (matcher, error) = match matcher {
                            Ok(matcher) => (matcher, None),
                            Err(error) => (None, Some(error)),
                        };
                        let matches = if error.is_none() {
                            request
                                .entries
                                .iter()
                                .enumerate()
                                .filter_map(|(index, entry)| {
                                    entry_matches(entry, &request.spec, matcher.as_ref())
                                        .then_some(index)
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        results.lock().unwrap().filter = Some(FilterResult {
                            generation: request.generation,
                            processed_len: request.entries.len(),
                            matches,
                            elapsed: started.elapsed(),
                            error,
                            matcher,
                        });
                    }
                }
                ctx.request_repaint();
            }
        })
        .expect("failed to start filter worker");
    (FilterSender(mailbox), FilterReceiver(completed))
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
    fn package_and_regex_are_combined() {
        let spec = FilterSpec {
            package: "com.example.app".to_owned(),
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
}
