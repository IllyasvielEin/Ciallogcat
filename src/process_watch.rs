//! Observed changes in one application's sampled process set; never lifecycle proof.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use serde::Serialize;

const MAX_PROCESSES: usize = 128;
const MAX_CHANGES: usize = 32;

#[derive(Clone, Debug, Serialize)]
pub(super) struct Change {
    pub kind: &'static str,
    pub pid: u32,
    pub name: String,
    pub name_truncated: bool,
    pub observed_unix_ms: Option<u64>,
}

impl Change {
    pub fn label(&self) -> &'static str {
        match self.kind {
            "first_observed" => "首次观察到",
            "appeared" => "观察到出现",
            _ => "本次未再观察到",
        }
    }
}

#[derive(Default)]
pub(super) struct Watcher {
    pub package: String,
    previous: Option<BTreeMap<u32, Arc<str>>>,
    pub recent: VecDeque<Change>,
    pub omitted_changes: u64,
    pub truncated_sample: bool,
    pub current_known: bool,
}

impl Watcher {
    pub fn invalidate(&mut self) {
        self.previous = None;
        self.current_known = false;
    }

    pub fn observe(&mut self, package: &str, processes: &HashMap<u32, Arc<str>>) -> Vec<Change> {
        let package = package.trim();
        if self.package != package {
            *self = Self {
                package: package.into(),
                ..Self::default()
            };
        }
        if package.is_empty() {
            return Vec::new();
        }
        let mut next = BTreeMap::new();
        self.truncated_sample = false;
        for (&pid, name) in processes.iter().filter(|(_, name)| {
            name.as_ref() == package
                || name
                    .strip_prefix(package)
                    .is_some_and(|suffix| suffix.starts_with(':'))
        }) {
            next.insert(pid, Arc::clone(name));
            if next.len() > MAX_PROCESSES {
                next.pop_last();
                self.truncated_sample = true;
            }
        }
        let mut changes = Vec::new();
        let mut add = |kind, pid, name: &str| {
            let mut end = name.len().min(256);
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            changes.push(Change {
                kind,
                pid,
                name: name[..end].into(),
                name_truncated: end < name.len(),
                observed_unix_ms: ciallogcat::capture_facts::unix_ms(),
            });
        };
        if !self.truncated_sample
            && let Some(previous) = &self.previous
        {
            for (&pid, name) in previous {
                if next.get(&pid) != Some(name) {
                    add("no_longer_observed", pid, name);
                }
            }
        }
        for (&pid, name) in &next {
            if self
                .previous
                .as_ref()
                .and_then(|previous| previous.get(&pid))
                != Some(name)
            {
                add(
                    if self.previous.is_some() {
                        "appeared"
                    } else {
                        "first_observed"
                    },
                    pid,
                    name,
                );
            }
        }
        self.current_known = true;
        // A capped observation cannot establish disappearances in the next one.
        self.previous = (!self.truncated_sample).then_some(next);
        for change in &changes {
            if self.recent.len() == MAX_CHANGES {
                self.recent.pop_front();
                self.omitted_changes = self.omitted_changes.saturating_add(1);
            }
            self.recent.push_back(change.clone());
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(rows: &[(u32, &str)]) -> HashMap<u32, Arc<str>> {
        rows.iter()
            .map(|(pid, name)| (*pid, Arc::from(*name)))
            .collect()
    }

    #[test]
    fn tracks_restarts_and_children_without_treating_failed_samples_as_exits() {
        let mut watcher = Watcher::default();
        let first = watcher.observe(
            "com.app",
            &sample(&[
                (1, "com.app"),
                (2, "com.app:worker"),
                (3, "com.application"),
            ]),
        );
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|change| change.kind == "first_observed"));
        let restart = watcher.observe("com.app", &sample(&[(5, "com.app"), (6, "com.app:worker")]));
        assert_eq!(
            restart
                .iter()
                .filter(|change| change.kind == "no_longer_observed")
                .count(),
            2
        );
        assert_eq!(
            restart
                .iter()
                .filter(|change| change.kind == "appeared")
                .count(),
            2
        );
        watcher.invalidate();
        assert!(!watcher.current_known);
        let recovered = watcher.observe("com.app", &sample(&[(9, "com.app")]));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].kind, "first_observed");
        let switched = watcher.observe("com.other", &sample(&[(9, "com.other")]));
        assert_eq!(switched.len(), 1);
        assert_eq!(watcher.recent.len(), 1);
    }

    #[test]
    fn capped_sample_and_history_expose_their_limits() {
        let mut watcher = Watcher::default();
        let many: HashMap<_, _> = (0..200).map(|pid| (pid, Arc::from("com.app"))).collect();
        watcher.observe("com.app", &many);
        assert!(watcher.truncated_sample);
        assert_eq!(watcher.recent.len(), 32);
        assert_eq!(watcher.omitted_changes, 96);
        let next = watcher.observe("com.app", &sample(&[(299, "com.app")]));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].kind, "first_observed");
    }
}
