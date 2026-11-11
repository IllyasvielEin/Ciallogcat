//! Process-name attribution for records that arrive without one (text transport).
//!
//! Two sources are combined, both explicitly labelled as hints:
//!
//! - `log_lifecycle`: framework lines in the same ordered log stream. An
//!   `ActivityManager` `Start proc PID:NAME/...` line names a PID until the
//!   matching `Process NAME (pid PID) has died` line or a newer start for that
//!   PID; `AndroidRuntime` `Process: NAME, PID: PID` (crash header) names it too.
//!   This catches short-lived processes that start and crash between `ps`
//!   samples, which is common during app startup.
//! - `ps_snapshot`: a recent `ps` sample, applied with the same freshness rule
//!   as the desktop viewer (only records received after a sample completed and
//!   within [`PS_HINT_TTL`] of its start). Callers enable this only for live
//!   follow captures; it never names historical rows.
//!
//! Neither source proves process-instance or package ownership; PIDs can be
//! reused and lifecycle lines can be missed.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::model::{LogEntry, LogEntryKind, ProcessSource};

/// Same freshness window as the desktop viewer's ps hints.
pub const PS_HINT_TTL: Duration = Duration::from_secs(2);
const MAX_STARTED: usize = 4096;
const MAX_NAME_BYTES: usize = 512;

#[derive(Default)]
pub struct ProcessNames {
    started: HashMap<u32, Arc<str>>,
    ps: HashMap<u32, Arc<str>>,
    ps_window: Option<(Instant, Instant)>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `Start proc 1234:com.example:remote/u0a12 for service {...}` (Android 8+)
/// or `Start proc com.example for activity ...: pid=1234 uid=...` (older).
fn parse_start(message: &str) -> Option<(u32, &str)> {
    let rest = message.strip_prefix("Start proc ")?;
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        let (pid, rest) = rest.split_once(':')?;
        let name = rest.split(['/', ' ']).next()?;
        Some((pid.parse().ok()?, name))
    } else {
        let name = rest.split(' ').next()?;
        let pid = rest
            .split_once(" pid=")?
            .1
            .split(|c: char| !c.is_ascii_digit())
            .next()?;
        Some((pid.parse().ok()?, name))
    }
}

/// `Process com.example (pid 1234) has died: ...`
fn parse_death(message: &str) -> Option<(u32, &str)> {
    let rest = message.strip_prefix("Process ")?;
    let (name, rest) = rest.split_once(" (pid ")?;
    let (pid, rest) = rest.split_once(')')?;
    rest.trim_start().starts_with("has died").then_some(())?;
    Some((pid.parse().ok()?, name))
}

/// `Process: com.example, PID: 1234` in an AndroidRuntime crash header.
fn parse_crash_header(message: &str) -> Option<(u32, &str)> {
    let rest = message.strip_prefix("Process: ")?;
    let (name, rest) = rest.split_once(", PID: ")?;
    let pid = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((pid.parse().ok()?, name))
}

impl ProcessNames {
    /// Learn from framework lifecycle lines. Call for every record, in stream
    /// order, before [`Self::attribute`].
    pub fn observe(&mut self, entry: &LogEntry) {
        if entry.kind != LogEntryKind::Log {
            return;
        }
        let first = entry.message.lines().next().unwrap_or_default().trim();
        match entry.tag.as_ref() {
            "ActivityManager" => {
                if let Some((pid, name)) = parse_start(first) {
                    self.learn(pid, name);
                } else if let Some((pid, name)) = parse_death(first)
                    && self
                        .started
                        .get(&pid)
                        .is_some_and(|known| known.as_ref() == name)
                {
                    self.started.remove(&pid);
                }
            }
            "AndroidRuntime" => {
                for line in entry.message.lines().take(8) {
                    if let Some((pid, name)) = parse_crash_header(line.trim()) {
                        self.learn(pid, name);
                    }
                }
            }
            _ => {}
        }
    }

    fn learn(&mut self, pid: u32, name: &str) {
        if pid == 0 || !valid_name(name) {
            return;
        }
        if self.started.len() >= MAX_STARTED && !self.started.contains_key(&pid) {
            return;
        }
        self.started.insert(pid, Arc::from(name));
    }

    /// Replace the ps sample. `None` (a failed sample) discards hints. A fresh
    /// sample that names a PID differently reveals a missed death line.
    pub fn update_ps(
        &mut self,
        sample: Option<HashMap<u32, String>>,
        observed_from: Instant,
        observed_until: Instant,
    ) {
        self.ps.clear();
        self.ps_window = None;
        let Some(sample) = sample.filter(|_| observed_until >= observed_from) else {
            return;
        };
        for (pid, name) in sample {
            if !valid_name(&name) {
                continue;
            }
            if self
                .started
                .get(&pid)
                .is_some_and(|known| known.as_ref() != name)
            {
                self.started.remove(&pid);
            }
            self.ps.insert(pid, Arc::from(name));
        }
        self.ps_window = Some((observed_from, observed_until));
    }

    /// Drop the ps sample (failed, expired or invalidated) and keep lifecycle
    /// names, which do not depend on sampling.
    pub fn discard_ps(&mut self) {
        self.ps.clear();
        self.ps_window = None;
    }

    /// Name a record that has no process yet. `ps_allowed` must be false for
    /// historical rows; lifecycle names follow stream order and always apply.
    pub fn attribute(&self, entry: &mut LogEntry, received_at: Instant, ps_allowed: bool) {
        if entry.kind != LogEntryKind::Log
            || entry.pid == 0
            || entry.process.is_some()
            || entry.proto.is_some()
        {
            return;
        }
        if let Some(name) = self.started.get(&entry.pid) {
            entry.process = Some(Arc::clone(name));
            entry.process_source = Some(ProcessSource::LogLifecycle);
        } else if ps_allowed
            && let Some((from, until)) = self.ps_window
            && received_at >= until
            && received_at
                .checked_duration_since(from)
                .is_some_and(|age| age <= PS_HINT_TTL)
            && let Some(name) = self.ps.get(&entry.pid)
        {
            entry.process = Some(Arc::clone(name));
            entry.process_source = Some(ProcessSource::PsSnapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Level;

    fn log(pid: u32, tag: &str, message: &str) -> LogEntry {
        LogEntry::new("", pid, pid, tag, Level::Info, message)
    }

    #[test]
    fn lifecycle_lines_name_processes_until_death() {
        let mut names = ProcessNames::default();
        let now = Instant::now();
        for message in [
            "Start proc 4200:com.android.chrome:sandboxed_process0:org.chromium.content.app.SandboxedProcessService0:0/u0i7 for  {com.android.chrome/org.chromium.content.app.SandboxedProcessService0:0}",
            "Start proc 4100:com.example.app/u0a123 for pre-top-activity {com.example.app/com.example.app.MainActivity}",
            "Start proc com.old.app for activity com.old.app/.Main: pid=3000 uid=10050 gids={50050}",
        ] {
            names.observe(&log(1000, "ActivityManager", message));
        }
        let mut chrome = log(4200, "cr_ChildProcessService", "renderer up");
        names.attribute(&mut chrome, now, false);
        assert_eq!(
            chrome.process.as_deref(),
            Some(
                "com.android.chrome:sandboxed_process0:org.chromium.content.app.SandboxedProcessService0:0"
            )
        );
        assert_eq!(chrome.process_source, Some(ProcessSource::LogLifecycle));
        assert!(chrome.matches_package("com.android.chrome"));
        let mut old = log(3000, "Old", "x");
        names.attribute(&mut old, now, false);
        assert_eq!(old.process.as_deref(), Some("com.old.app"));

        // A death line for another name does not clear the PID.
        names.observe(&log(
            1000,
            "ActivityManager",
            "Process com.other (pid 4100) has died: fg TOP",
        ));
        let mut app = log(4100, "App", "still here");
        names.attribute(&mut app, now, false);
        assert_eq!(app.process.as_deref(), Some("com.example.app"));
        names.observe(&log(
            1000,
            "ActivityManager",
            "Process com.example.app (pid 4100) has died: fg TOP",
        ));
        let mut reused = log(4100, "Daemon", "pid reused");
        names.attribute(&mut reused, now, false);
        assert!(reused.process.is_none());
    }

    #[test]
    fn crash_header_names_a_process_missed_by_sampling() {
        let mut names = ProcessNames::default();
        let mut header = log(
            777,
            "AndroidRuntime",
            "FATAL EXCEPTION: main\nProcess: com.example.fast, PID: 777",
        );
        names.observe(&header);
        names.attribute(&mut header, Instant::now(), false);
        assert_eq!(header.process.as_deref(), Some("com.example.fast"));
    }

    #[test]
    fn ps_hints_follow_the_freshness_rule_and_override_stale_lifecycle_names() {
        let mut names = ProcessNames::default();
        names.observe(&log(
            1000,
            "ActivityManager",
            "Start proc 10:com.gone/u0a1 for x",
        ));
        let from = Instant::now();
        let until = from + Duration::from_millis(50);
        names.update_ps(
            Some(HashMap::from([
                (10, "com.reused".to_string()),
                (20, "com.example.app".to_string()),
            ])),
            from,
            until,
        );
        let mut reused = log(10, "T", "m");
        names.attribute(&mut reused, until, true);
        assert_eq!(reused.process.as_deref(), Some("com.reused"));
        assert_eq!(reused.process_source, Some(ProcessSource::PsSnapshot));
        let mut history = log(20, "T", "m");
        names.attribute(&mut history, until, false);
        assert!(history.process.is_none(), "history rows never get ps hints");
        let mut early = log(20, "T", "m");
        names.attribute(&mut early, from, true);
        assert!(
            early.process.is_none(),
            "received before the sample completed"
        );
        let mut late = log(20, "T", "m");
        names.attribute(
            &mut late,
            from + PS_HINT_TTL + Duration::from_millis(1),
            true,
        );
        assert!(late.process.is_none(), "expired sample");
        names.update_ps(None, from, until);
        let mut failed = log(20, "T", "m");
        names.attribute(&mut failed, until, true);
        assert!(failed.process.is_none());
    }

    #[test]
    fn malformed_lines_are_ignored() {
        let mut names = ProcessNames::default();
        for message in [
            "Start proc x:com.bad/u0a1 for y",
            "Start proc 0:com.zero/u0a1 for y",
            "Start proc 5:/u0a1 for y",
            "Process com.x (pid nope) has died",
        ] {
            names.observe(&log(1000, "ActivityManager", message));
        }
        assert!(names.started.is_empty());
    }
}
