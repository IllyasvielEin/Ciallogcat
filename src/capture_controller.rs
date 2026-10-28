//! Capture intent and attempt lifecycle, independent of subprocesses and egui.
use std::time::{Duration, Instant};

use crate::model::LogBuffer;

pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const STABLE_RUN: Duration = Duration::from_secs(30);
const MAX_RETRIES: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub serial: String,
    pub buffers: Vec<LogBuffer>,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub enum Intent {
    Capture(Target),
    Paused,
    #[default]
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Start { id: u64, target: Target },
    Stop { id: u64 },
}

#[derive(Clone, Debug)]
struct Attempt {
    id: u64,
    target: Target,
    running_since: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
enum Phase {
    #[default]
    Idle,
    Active(Attempt),
    Stopping {
        attempt: Attempt,
        deadline: Instant,
        timed_out: bool,
    },
    Backoff {
        deadline: Instant,
        error: String,
    },
    Failed(String),
}

pub enum Status<'a> {
    Closed,
    Paused,
    Waiting,
    Starting,
    Running,
    Stopping,
    Backoff {
        deadline: Instant,
        retry: u8,
        error: &'a str,
    },
    Failed(&'a str),
}

#[derive(Default)]
pub struct Controller {
    intent: Intent,
    phase: Phase,
    next_id: u64,
    retries: u8,
    failure_after_stop: Option<String>,
}

impl Controller {
    pub fn intent(&self) -> &Intent {
        &self.intent
    }

    /// User actions supersede retry intent, but never release an unconfirmed
    /// child or extend its stop deadline. Repeated switches coalesce here.
    pub fn request(&mut self, intent: Intent, restart: bool, now: Instant) -> Option<Command> {
        self.intent = intent;
        self.retries = 0;
        match &self.phase {
            Phase::Active(attempt)
                if restart || self.intent != Intent::Capture(attempt.target.clone()) =>
            {
                let attempt = attempt.clone();
                let id = attempt.id;
                self.phase = Phase::Stopping {
                    attempt,
                    deadline: now + STOP_TIMEOUT,
                    timed_out: false,
                };
                Some(Command::Stop { id })
            }
            Phase::Active(_) | Phase::Stopping { .. } => None,
            _ => {
                self.phase = Phase::Idle;
                None
            }
        }
    }

    pub fn poll(&mut self, now: Instant, target_ready: bool) -> Option<Command> {
        if let Phase::Stopping {
            deadline,
            timed_out,
            ..
        } = &mut self.phase
        {
            if now >= *deadline {
                *timed_out = true;
            }
            return None;
        }
        let ready = match &self.phase {
            Phase::Idle => true,
            Phase::Backoff { deadline, .. } => now >= *deadline,
            _ => false,
        };
        if ready
            && target_ready
            && let Intent::Capture(target) = &self.intent
        {
            let Some(next_id) = self.next_id.checked_add(1) else {
                self.phase = Phase::Failed("采集 attempt 计数已耗尽。".to_owned());
                return None;
            };
            self.next_id = next_id;
            let target = target.clone();
            let id = self.next_id;
            self.phase = Phase::Active(Attempt {
                id,
                target: target.clone(),
                running_since: None,
            });
            return Some(Command::Start { id, target });
        }
        None
    }

    pub fn started(&mut self, id: u64, now: Instant) -> bool {
        if let Phase::Active(attempt) | Phase::Stopping { attempt, .. } = &mut self.phase
            && attempt.id == id
            && attempt.running_since.is_none()
        {
            attempt.running_since = Some(now);
            return true;
        }
        false
    }

    pub fn halt(&mut self, error: String, now: Instant) -> Option<Command> {
        let command = self.request(Intent::Paused, false, now);
        if self.is_stopping() {
            self.failure_after_stop = Some(error);
        } else {
            self.phase = Phase::Failed(error);
        }
        command
    }

    /// Returns true only for the current child. A stopping child's Exited is
    /// still authoritative even when its log data belongs to a replaced source.
    pub fn exited(&mut self, id: u64, error: String, now: Instant) -> bool {
        if self.active_id() != Some(id) {
            return false;
        }
        match std::mem::take(&mut self.phase) {
            Phase::Stopping {
                timed_out,
                deadline,
                ..
            } => {
                if let Some(error) = self.failure_after_stop.take() {
                    self.phase = Phase::Failed(error);
                } else if timed_out || now >= deadline {
                    self.phase =
                        Phase::Failed("停止确认曾超时，旧采集现已退出；请手动重试。".to_owned());
                }
            }
            Phase::Active(attempt) => {
                if attempt
                    .running_since
                    .is_some_and(|since| now.duration_since(since) >= STABLE_RUN)
                {
                    self.retries = 0;
                }
                if matches!(self.intent, Intent::Capture(_)) {
                    if self.retries < MAX_RETRIES {
                        let delay = Duration::from_secs(1 << self.retries);
                        self.retries += 1;
                        self.phase = Phase::Backoff {
                            deadline: now + delay,
                            error,
                        };
                    } else {
                        self.phase = Phase::Failed(format!("自动重连已尝试 3 次：{error}"));
                    }
                }
            }
            _ => unreachable!("active_id requires an active or stopping attempt"),
        }
        true
    }

    /// A finite capture ended successfully, with readers drained and child
    /// reaped. The caller checks the scope, exit code and absence of errors.
    /// A concurrent stop/replacement retains its intent and timeout semantics.
    pub fn completed(&mut self, id: u64, now: Instant) -> bool {
        if self.active_id() != Some(id) {
            return false;
        }
        if matches!(self.phase, Phase::Active(_)) {
            self.intent = Intent::Paused;
            self.retries = 0;
        }
        self.exited(id, String::new(), now)
    }

    pub fn active_id(&self) -> Option<u64> {
        match &self.phase {
            Phase::Active(attempt) | Phase::Stopping { attempt, .. } => Some(attempt.id),
            _ => None,
        }
    }

    pub fn active_target(&self) -> Option<&Target> {
        match &self.phase {
            Phase::Active(attempt) | Phase::Stopping { attempt, .. } => Some(&attempt.target),
            _ => None,
        }
    }

    pub fn is_stopping(&self) -> bool {
        matches!(self.phase, Phase::Stopping { .. })
    }

    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        match &self.phase {
            Phase::Stopping {
                deadline,
                timed_out: false,
                ..
            } => Some(*deadline),
            Phase::Backoff { deadline, .. } if *deadline > now => Some(*deadline),
            _ => None,
        }
    }

    pub fn status(&self) -> Status<'_> {
        match &self.phase {
            Phase::Idle => match self.intent {
                Intent::Capture(_) => Status::Waiting,
                Intent::Paused => Status::Paused,
                Intent::Closed => Status::Closed,
            },
            Phase::Active(attempt) if attempt.running_since.is_some() => Status::Running,
            Phase::Active(_) => Status::Starting,
            Phase::Stopping { .. } if self.failure_after_stop.is_some() => {
                Status::Failed(self.failure_after_stop.as_deref().unwrap())
            }
            Phase::Stopping {
                timed_out: true, ..
            } => Status::Failed("等待旧采集退出超过 5 秒；新采集不会启动。仍在等待停止确认。"),
            Phase::Stopping { .. } => Status::Stopping,
            Phase::Backoff { deadline, error } => Status::Backoff {
                deadline: *deadline,
                retry: self.retries,
                error,
            },
            Phase::Failed(error) => Status::Failed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(serial: &str) -> Target {
        Target {
            serial: serial.to_owned(),
            buffers: LogBuffer::defaults(),
        }
    }
    fn start(c: &mut Controller, now: Instant) -> u64 {
        let Some(Command::Start { id, .. }) = c.poll(now, true) else {
            panic!("expected start")
        };
        id
    }

    #[test]
    fn switches_coalesce_and_only_exit_releases_the_old_attempt() {
        let now = Instant::now();
        let mut c = Controller::default();
        c.request(Intent::Capture(target("a")), false, now);
        let old = start(&mut c, now);
        assert_eq!(
            c.request(Intent::Capture(target("b")), false, now),
            Some(Command::Stop { id: old })
        );
        assert!(
            c.request(Intent::Capture(target("c")), false, now)
                .is_none()
        );
        assert!(c.poll(now, true).is_none());
        assert!(!c.exited(old + 1, "stale".into(), now));
        assert!(c.exited(old, "stopped".into(), now));
        assert!(
            matches!(c.poll(now, true), Some(Command::Start { target: Target { serial, .. }, .. }) if serial == "c")
        );
    }

    #[test]
    fn finite_completion_stops_intent_but_preserves_a_concurrent_replacement() {
        let now = Instant::now();
        let mut c = Controller::default();
        c.request(Intent::Capture(target("a")), false, now);
        let id = start(&mut c, now);
        assert!(!c.completed(id + 1, now));
        assert_eq!(c.active_id(), Some(id));
        assert!(c.completed(id, now));
        assert!(matches!(c.status(), Status::Paused));
        assert!(c.poll(now + Duration::from_secs(60), true).is_none());
        c.request(Intent::Capture(target("a")), false, now);
        let old = start(&mut c, now);
        c.request(Intent::Capture(target("b")), false, now);
        assert!(c.completed(old, now));
        assert!(
            matches!(c.poll(now, true), Some(Command::Start { target: Target { serial, .. }, .. }) if serial == "b")
        );
    }

    #[test]
    fn finite_completion_does_not_erase_stop_failures() {
        for halt in [false, true] {
            let now = Instant::now();
            let mut c = Controller::default();
            c.request(Intent::Capture(target("a")), false, now);
            let id = start(&mut c, now);
            if halt {
                c.halt("output failed".into(), now);
            } else {
                c.request(Intent::Capture(target("b")), false, now);
            }
            assert!(c.completed(id, if halt { now } else { now + STOP_TIMEOUT }));
            assert!(matches!(c.status(), Status::Failed(_)));
            assert!(c.poll(now + STOP_TIMEOUT, true).is_none());
        }
    }

    #[test]
    fn flapping_is_limited_to_three_retries_and_started_does_not_reset_budget() {
        let mut now = Instant::now();
        let mut c = Controller::default();
        c.request(Intent::Capture(target("a")), false, now);
        let mut id = start(&mut c, now);
        for seconds in [1, 2, 4] {
            c.started(id, now);
            c.exited(id, "disconnected".into(), now);
            assert!(c.poll(now + Duration::from_millis(999), true).is_none());
            now += Duration::from_secs(seconds);
            id = start(&mut c, now);
        }
        c.started(id, now);
        c.exited(id, "disconnected".into(), now);
        assert!(matches!(c.status(), Status::Failed(_)));
        assert!(c.poll(now + Duration::from_secs(600), true).is_none());
    }

    #[test]
    fn pause_and_close_cancel_backoff_and_stopping_timeout_never_starts_replacement() {
        for intent in [Intent::Paused, Intent::Closed] {
            let now = Instant::now();
            let mut c = Controller::default();
            c.request(Intent::Capture(target("a")), false, now);
            let id = start(&mut c, now);
            c.exited(id, "disconnected".into(), now);
            c.request(intent, false, now);
            assert!(c.poll(now + Duration::from_secs(10), true).is_none());
        }
        let now = Instant::now();
        let mut c = Controller::default();
        c.request(Intent::Capture(target("a")), false, now);
        let old = start(&mut c, now);
        c.request(Intent::Capture(target("b")), false, now);
        c.poll(now + STOP_TIMEOUT, true);
        assert!(matches!(c.status(), Status::Failed(_)));
        c.request(Intent::Capture(target("c")), false, now + STOP_TIMEOUT);
        assert!(c.poll(now + STOP_TIMEOUT, true).is_none());
        c.exited(old, "late exit".into(), now + STOP_TIMEOUT);
        assert!(c.poll(now + STOP_TIMEOUT, true).is_none());
        c.request(Intent::Capture(target("c")), false, now + STOP_TIMEOUT);
        assert!(c.poll(now + STOP_TIMEOUT, true).is_some());
    }

    #[test]
    fn stable_run_restores_retry_budget_and_missing_device_does_not_spend_it() {
        let now = Instant::now();
        let mut c = Controller::default();
        c.request(Intent::Capture(target("a")), false, now);
        let old = start(&mut c, now);
        c.exited(old, "first failure".into(), now);
        assert!(c.poll(now + Duration::from_secs(10), false).is_none());
        let id = start(&mut c, now + Duration::from_secs(10));
        c.started(id, now + Duration::from_secs(10));
        let end = now + Duration::from_secs(10) + STABLE_RUN;
        c.exited(id, "later failure".into(), end);
        assert!(matches!(c.status(), Status::Backoff { retry: 1, .. }));
        assert!(c.poll(end + Duration::from_secs(1), true).is_some());
    }

    #[test]
    fn late_exit_without_an_intermediate_timer_tick_does_not_restart() {
        let now = Instant::now();
        let mut c = Controller::default();
        c.request(Intent::Capture(target("a")), false, now);
        let old = start(&mut c, now);
        c.request(Intent::Capture(target("b")), false, now);
        c.exited(old, "late exit".into(), now + STOP_TIMEOUT);
        assert!(matches!(c.status(), Status::Failed(_)));
        assert!(c.poll(now + STOP_TIMEOUT, true).is_none());
    }
}
