use std::sync::{Arc, Condvar, Mutex, mpsc::Sender};
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
}

#[derive(Default)]
struct PendingFilter {
    request: Option<FilterRequest>,
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
}
impl Drop for FilterSender {
    fn drop(&mut self) {
        let mut pending = self.0.pending.lock().unwrap();
        pending.closed = true;
        pending.request = None;
        self.0.ready.notify_one();
    }
}

pub fn spawn_filter_worker(
    result_tx: Sender<FilterResult>,
    ctx: eframe::egui::Context,
) -> FilterSender {
    let mailbox = Arc::new(FilterMailbox::default());
    let receiver = Arc::clone(&mailbox);
    std::thread::Builder::new()
        .name("log-filter".to_owned())
        .spawn(move || {
            loop {
                let request = {
                    let mut pending = receiver.pending.lock().unwrap();
                    while pending.request.is_none() && !pending.closed {
                        pending = receiver.ready.wait(pending).unwrap();
                    }
                    if pending.closed {
                        break;
                    }
                    pending.request.take().unwrap()
                };

                let started = Instant::now();
                let matcher = match compile_matcher(&request.spec) {
                    Ok(matcher) => matcher,
                    Err(error) => {
                        let _ = result_tx.send(FilterResult {
                            generation: request.generation,
                            processed_len: request.entries.len(),
                            matches: Vec::new(),
                            elapsed: started.elapsed(),
                            error: Some(error),
                        });
                        ctx.request_repaint();
                        continue;
                    }
                };

                let matches = request
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        entry_matches(entry, &request.spec, matcher.as_ref()).then_some(index)
                    })
                    .collect();

                let _ = result_tx.send(FilterResult {
                    generation: request.generation,
                    processed_len: request.entries.len(),
                    matches,
                    elapsed: started.elapsed(),
                    error: None,
                });
                ctx.request_repaint();
            }
        })
        .expect("failed to start filter worker");
    FilterSender(mailbox)
}

pub fn compile_matcher(spec: &FilterSpec) -> Result<Option<Regex>, String> {
    if spec.query.is_empty() {
        return Ok(None);
    }

    let pattern = if spec.regex {
        spec.query.clone()
    } else {
        regex::escape(&spec.query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!spec.case_sensitive)
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
