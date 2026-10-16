//! Shared, headless Android Logcat backend.
//!
//! Build with `--no-default-features` to omit the desktop UI dependencies.
//! Consumers receive backend events through bounded channels. [`WakeSignal`] is
//! only a best-effort hint; receive events directly or poll independently rather
//! than relying solely on notifications. Polling, retention policy and session
//! ownership remain the responsibility of the consumer.

pub mod adb;
pub mod filter;
pub mod log_store;
pub mod model;
mod wake_signal;

pub use wake_signal::WakeSignal;
