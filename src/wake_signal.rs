use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// Cloneable, best-effort wake hint for backend or filter consumers.
///
/// Notifications may be coalesced or spurious; they are not event counts or
/// completion acknowledgements. Drain available results when woken. Producers
/// enqueue before notifying, and [`Self::coalesced`] preserves a trailing wake
/// even when the producer becomes silent. A consumer that limits its drain must
/// schedule continuation itself when it reaches that limit.
///
/// Callbacks may run concurrently on worker threads. They must be fast,
/// non-blocking and non-panicking, and should wake the consumer rather than
/// process results themselves.
#[derive(Clone)]
pub struct WakeSignal(Arc<dyn Fn() + Send + Sync>);

impl WakeSignal {
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }

    pub fn notify(&self) {
        (self.0)();
    }

    /// Merge notifications within `interval`, preserving the final wake.
    ///
    /// Uses one sleeping timer worker per returned signal, shared by its clones,
    /// and one pending deadline, regardless of event volume. Later notifications
    /// do not postpone that deadline. Delivery is subject to OS scheduling and
    /// callback latency; `interval` is not a hard real-time bound. Dropping the
    /// last clone flushes any pending wake before the worker exits.
    pub fn coalesced(&self, interval: Duration) -> Self {
        let state = Arc::new(TimerState::default());
        let worker = Arc::clone(&state);
        let target = self.clone();
        std::thread::Builder::new()
            .name("coalesced-wake".into())
            .spawn(move || {
                let mut pending = worker.pending.lock().unwrap();
                loop {
                    if let Some(deadline) = pending.deadline {
                        let delay = deadline.saturating_duration_since(Instant::now());
                        if !pending.closed && !delay.is_zero() {
                            pending = worker.ready.wait_timeout(pending, delay).unwrap().0;
                            continue;
                        }
                        pending.deadline = None;
                        drop(pending);
                        target.notify();
                        pending = worker.pending.lock().unwrap();
                    } else if pending.closed {
                        break;
                    } else {
                        pending = worker.ready.wait(pending).unwrap();
                    }
                }
            })
            .expect("failed to start wake timer");
        let timer = Timer { state, interval };
        Self::new(move || timer.notify())
    }
}

#[derive(Default)]
struct Pending {
    deadline: Option<Instant>,
    closed: bool,
}
#[derive(Default)]
struct TimerState {
    pending: Mutex<Pending>,
    ready: Condvar,
}
struct Timer {
    state: Arc<TimerState>,
    interval: Duration,
}
impl Timer {
    fn notify(&self) {
        let mut pending = self.state.pending.lock().unwrap();
        if pending.deadline.is_none() {
            pending.deadline = Some(Instant::now() + self.interval);
            self.state.ready.notify_one();
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        self.state.pending.lock().unwrap().closed = true;
        self.state.ready.notify_one();
    }
}

impl Default for WakeSignal {
    fn default() -> Self {
        Self::new(|| {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    #[test]
    fn single_entry_after_prior_wake_is_delivered_without_further_production() {
        let (tx, rx) = mpsc::channel();
        let wake = WakeSignal::new(move || {
            let _ = tx.send(());
        });
        let merged = wake.coalesced(Duration::from_millis(10));
        merged.notify();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        merged.notify();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn burst_preserves_final_state_and_coalesces_callbacks() {
        let produced = Arc::new(AtomicUsize::new(0));
        let observed = produced.clone();
        let (tx, rx) = mpsc::channel();
        let merged = WakeSignal::new(move || {
            let _ = tx.send(observed.load(Ordering::Acquire));
        })
        .coalesced(Duration::from_millis(10));
        for index in 1..=50_000 {
            produced.store(index, Ordering::Release);
            merged.notify();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut callbacks = 0;
        loop {
            let observed = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            callbacks += 1;
            if observed == 50_000 {
                break;
            }
        }
        assert!(callbacks < 50_000, "notifications must merge");
    }

    #[test]
    fn dropping_final_clone_flushes_pending_wake_without_waiting_for_interval() {
        let (tx, rx) = mpsc::channel();
        let merged = WakeSignal::new(move || {
            let _ = tx.send(());
        })
        .coalesced(Duration::from_secs(60));
        let clone = merged.clone();
        merged.notify();
        drop(merged);
        assert!(rx.try_recv().is_err());
        drop(clone);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
