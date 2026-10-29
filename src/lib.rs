//! Shared, headless Android Logcat backend.
//!
//! Build with `--no-default-features` to omit the desktop UI dependencies.
//! Consumers receive backend events through bounded channels. [`WakeSignal`] is
//! a coalescible hint: drain queued results when notified, and schedule another
//! drain if a local work budget is reached. Logcat entry notifications preserve
//! the final wake of a burst. Retention policy and session ownership remain the
//! responsibility of the consumer.

pub mod adb;
pub mod capture_controller;
pub mod capture_facts;
pub mod capture_scope;
pub mod diagnostics;
pub mod document;
pub mod file_io;
pub mod filter;
pub mod incident;
#[doc(hidden)]
pub mod json_input;
pub mod log_store;
pub mod model;
pub mod protobuf;
pub mod session;
mod wake_signal;

pub use wake_signal::WakeSignal;
