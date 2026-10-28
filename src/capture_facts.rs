//! Bounded host observations that survive view clearing and artifact selection.
//! These are capture facts, not device events or proof of loss-free recording.
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::document::{CaptureStatus, Extensions};
use crate::model::LogBuffer;

const RECENT_OBSERVATIONS: usize = 64;
const DIAGNOSTIC_BYTES: usize = 1024;

pub fn unix_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_millis()).ok())
}

#[derive(Default)]
pub struct CaptureFacts {
    attempt: u64,
    segment: Option<u64>,
    started: bool,
    intentional_stop: bool,
    resume_pending: bool,
    attempts: u64,
    interruptions: u64,
    start_failures: u64,
    resumed_pauses: u64,
    faults: u64,
    discarded_records: u64,
    retention_removed: u64,
    cleared_records: u64,
    omitted_observations: u64,
    recent: VecDeque<Value>,
}

impl CaptureFacts {
    pub fn begin_attempt(&mut self, attempt: u64, buffers: &[LogBuffer], after_seq: u64) {
        self.begin_attempt_with_scope(
            attempt,
            buffers,
            &crate::capture_scope::CaptureScope::default(),
            after_seq,
        );
    }

    pub fn begin_attempt_with_scope(
        &mut self,
        attempt: u64,
        buffers: &[LogBuffer],
        scope: &crate::capture_scope::CaptureScope,
        after_seq: u64,
    ) {
        self.attempt = attempt;
        self.segment = None;
        self.started = false;
        self.intentional_stop = false;
        self.attempts = self.attempts.saturating_add(1);
        if std::mem::take(&mut self.resume_pending) {
            self.resumed_pauses = self.resumed_pauses.saturating_add(1);
        }
        self.observe(
            "attempt_started",
            after_seq,
            json!({"requested_buffers":buffers,"transport":"threadtime","capture_scope":scope,"logcat_scope_args":scope.logcat_args()}),
        );
    }

    pub fn stream_started(&mut self, segment: u64, after_seq: u64) {
        self.started = true;
        self.segment = Some(segment);
        if self.intentional_stop {
            self.resume_pending = true;
        }
        self.observe("stream_started", after_seq, json!({}));
    }

    pub fn stop_requested(&mut self, reason: &str, after_seq: u64) {
        if self.intentional_stop {
            return;
        }
        self.intentional_stop = true;
        // A graceful final stop alone is not a missing interval. If capture
        // resumes in this same session, the intervening scope is unknown.
        self.resume_pending = self.started;
        self.observe("stop_requested", after_seq, json!({"reason":reason}));
    }

    pub fn exited(&mut self, code: Option<i32>, diagnostic: &str, after_seq: u64) {
        let kind = if !self.started {
            self.start_failures = self.start_failures.saturating_add(1);
            "start_failed"
        } else if !self.intentional_stop {
            self.interruptions = self.interruptions.saturating_add(1);
            "stream_interrupted"
        } else {
            "stream_stopped"
        };
        let end = diagnostic
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|&index| index <= DIAGNOSTIC_BYTES)
            .last()
            .unwrap_or(0);
        let end = if diagnostic.len() <= DIAGNOSTIC_BYTES {
            diagnostic.len()
        } else {
            end
        };
        self.observe(
            kind,
            after_seq,
            json!({"exit_code":code,"diagnostic":&diagnostic[..end],"diagnostic_truncated":end<diagnostic.len()}),
        );
    }

    pub fn fault(&mut self, after_seq: u64) {
        self.faults = self.faults.saturating_add(1);
        self.observe("host_capture_fault", after_seq, json!({}));
    }

    pub fn process_observation(&mut self, observation: Value, after_seq: u64) {
        self.observe("process_snapshot_change", after_seq, observation);
    }

    pub fn snapshot_completed(&mut self, after_seq: u64) {
        self.intentional_stop = true;
        // Completion alone is not a gap. A later capture in this session cannot
        // establish continuity across the interval without a device cursor.
        self.resume_pending = self.started;
        self.observe("snapshot_completed", after_seq, json!({}));
    }

    pub fn record_discarded(&mut self) {
        self.discarded_records = self.discarded_records.saturating_add(1);
    }

    pub fn retention_removed(&mut self, count: usize) {
        self.retention_removed = self.retention_removed.saturating_add(count as u64);
    }

    pub fn view_cleared(&mut self, count: usize, after_seq: u64) {
        if count == 0 {
            return;
        }
        self.cleared_records = self.cleared_records.saturating_add(count as u64);
        self.observe("local_view_cleared", after_seq, json!({"records":count}));
    }

    pub fn retention_scope(&self) -> Extensions {
        Extensions::from([
            (
                "retention_evicted_records".into(),
                json!(self.retention_removed),
            ),
            ("view_cleared_records".into(), json!(self.cleared_records)),
        ])
    }

    pub fn snapshot(&self, state: &str) -> CaptureStatus {
        let mut gaps = Vec::new();
        for (kind, count) in [
            ("stream_interruptions", self.interruptions),
            ("capture_start_failures", self.start_failures),
            ("resumed_host_pauses", self.resumed_pauses),
            ("host_capture_faults", self.faults),
        ] {
            if count > 0 {
                gaps.push(json!({"kind":kind,"count":count,"loss":"unknown"}));
            }
        }
        if self.discarded_records > 0 {
            gaps.push(json!({"kind":"host_discarded_records","count":self.discarded_records}));
        }
        CaptureStatus {
            state: state.into(),
            completeness: if gaps.is_empty() {
                "unknown"
            } else {
                "partial"
            }
            .into(),
            observed_unix_ms: unix_ms(),
            known_gaps: gaps,
            extra: Extensions::from([
                ("attempts".into(), json!(self.attempts)),
                ("recent_observations".into(), json!(self.recent)),
                (
                    "omitted_observations".into(),
                    json!(self.omitted_observations),
                ),
                ("observation_clock".into(), json!("host_wall_clock")),
                (
                    "after_seq_semantics".into(),
                    json!("last_host_admitted_record_not_device_time_boundary"),
                ),
            ]),
        }
    }

    fn observe(&mut self, kind: &str, after_seq: u64, details: Value) {
        if self.recent.len() == RECENT_OBSERVATIONS {
            self.recent.pop_front();
            self.omitted_observations = self.omitted_observations.saturating_add(1);
        }
        self.recent.push_back(json!({
            "kind":kind,"attempt":self.attempt,"segment":self.segment,
            "after_seq":after_seq,"observed_unix_ms":unix_ms(),"details":details,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interruption_survives_bounded_history_and_view_changes() {
        let mut facts = CaptureFacts::default();
        for attempt in 1..=100 {
            facts.begin_attempt(attempt, &LogBuffer::defaults(), attempt - 1);
            facts.stream_started(attempt, attempt - 1);
            facts.exited(Some(17), &"失".repeat(1000), attempt);
        }
        facts.retention_removed(40);
        facts.view_cleared(60, 100);
        let snapshot = facts.snapshot("reconnecting");
        assert_eq!(snapshot.completeness, "partial");
        assert_eq!(snapshot.known_gaps[0]["count"], 100);
        assert_eq!(
            snapshot.extra["recent_observations"]
                .as_array()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(snapshot.extra["omitted_observations"], 237);
        assert!(serde_json::to_vec(&snapshot).unwrap().len() < 64 * 2048);
        assert_eq!(facts.retention_scope()["retention_evicted_records"], 40);
        assert_eq!(facts.retention_scope()["view_cleared_records"], 60);
    }

    #[test]
    fn final_stop_is_not_an_interruption_but_resume_has_unknown_gap() {
        let mut facts = CaptureFacts::default();
        facts.begin_attempt(1, &LogBuffer::defaults(), 0);
        facts.stream_started(1, 0);
        facts.stop_requested("user_pause", 10);
        facts.stop_requested("user_pause", 10);
        facts.exited(None, "host stopped", 11);
        assert!(facts.snapshot("paused").known_gaps.is_empty());
        facts.begin_attempt(2, &[LogBuffer::Crash], 11);
        facts.exited(Some(1), "permission denied", 11);
        let snapshot = facts.snapshot("failed");
        assert_eq!(snapshot.completeness, "partial");
        assert_eq!(snapshot.known_gaps.len(), 2);
        assert_eq!(snapshot.known_gaps[1]["kind"], "resumed_host_pauses");
        assert_eq!(snapshot.known_gaps[1]["count"], 1);
        assert!(
            !snapshot
                .known_gaps
                .iter()
                .any(|gap| gap["kind"] == "stream_interruptions")
        );
    }

    #[test]
    fn started_arriving_after_stop_request_still_records_resume_gap() {
        let mut facts = CaptureFacts::default();
        facts.begin_attempt(1, &LogBuffer::defaults(), 0);
        facts.stop_requested("user_pause", 0);
        facts.stream_started(1, 0);
        facts.exited(None, "stopped after draining", 2);
        assert!(facts.snapshot("paused").known_gaps.is_empty());
        facts.begin_attempt(2, &LogBuffer::defaults(), 2);
        assert!(
            facts
                .snapshot("starting")
                .known_gaps
                .iter()
                .any(|gap| gap["kind"] == "resumed_host_pauses" && gap["count"] == 1)
        );
    }
}
