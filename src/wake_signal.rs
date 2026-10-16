use std::sync::Arc;

/// Cloneable, best-effort wake hint for backend or filter consumers.
///
/// This is not a readiness or completion guarantee. Notifications may be
/// coalesced or spurious; in particular, Logcat entry wakes are throttled without
/// a trailing timer. An entry may arrive after a wake and then remain idle
/// without another wake. Backend consumers must receive with `recv` or
/// `recv_timeout`, or run an independent periodic pump; never wait solely for
/// this callback. Drain available results when woken.
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
}

impl Default for WakeSignal {
    fn default() -> Self {
        Self::new(|| {})
    }
}
