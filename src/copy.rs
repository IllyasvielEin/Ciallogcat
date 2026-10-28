//! Bounded full-record clipboard serialization. Classification readiness belongs
//! to the app; this helper validates indices and sizes before allocating output.

use std::fmt;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
};

use crate::log_store::LogStore;
use ciallogcat::WakeSignal;

/// Provisional product ceiling for one clipboard request, independent of the
/// retention budget. This bounds output bytes, not native clipboard/RSS overhead.
pub const MAX_COPY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyError {
    InvalidSelection,
    TooLarge {
        limit_bytes: usize,
        minimum_bytes: usize,
    },
    SizeOverflow,
    AllocationFailed,
    Cancelled,
}

impl fmt::Display for CopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSelection => formatter
                .write_str("The selected logs are no longer available. Select the logs again."),
            Self::TooLarge {
                limit_bytes,
                minimum_bytes,
            } => write!(
                formatter,
                "Selected logs need at least {minimum_bytes} bytes, exceeding the \
                 clipboard limit of {limit_bytes} bytes. Select fewer rows."
            ),
            Self::SizeOverflow => formatter
                .write_str("The selected logs are too large to size safely. Select fewer rows."),
            Self::AllocationFailed => formatter
                .write_str("There is not enough memory to copy these logs. Select fewer rows."),
            Self::Cancelled => formatter.write_str("Copy preparation was cancelled."),
        }
    }
}

impl std::error::Error for CopyError {}

/// Copies complete records in iterator order, with one newline between records.
/// No formatted row strings or copied index list are created during preflight.
pub fn build_copy(
    entries: &LogStore,
    indices: impl Iterator<Item = usize> + Clone,
    limit: usize,
) -> Result<String, CopyError> {
    build_copy_with_reserve(entries, indices, limit, |text, bytes| {
        text.try_reserve_exact(bytes)
            .map_err(|_| CopyError::AllocationFailed)
    })
}

fn add_row_bytes(
    total: usize,
    row_bytes: usize,
    separator: bool,
    limit: usize,
) -> Result<usize, CopyError> {
    let next = total
        .checked_add(usize::from(separator))
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or(CopyError::SizeOverflow)?;
    if next > limit {
        return Err(CopyError::TooLarge {
            limit_bytes: limit,
            minimum_bytes: next,
        });
    }
    Ok(next)
}

fn build_copy_with_reserve(
    entries: &LogStore,
    indices: impl Iterator<Item = usize> + Clone,
    limit: usize,
    reserve: impl FnOnce(&mut String, usize) -> Result<(), CopyError>,
) -> Result<String, CopyError> {
    build_cancellable(entries, indices, limit, reserve, || false)
}

fn build_cancellable(
    entries: &LogStore,
    indices: impl Iterator<Item = usize> + Clone,
    limit: usize,
    reserve: impl FnOnce(&mut String, usize) -> Result<(), CopyError>,
    cancelled: impl Fn() -> bool,
) -> Result<String, CopyError> {
    let mut total = 0;
    let mut separator = false;
    for index in indices.clone() {
        if cancelled() {
            return Err(CopyError::Cancelled);
        }
        if index >= entries.len() {
            return Err(CopyError::InvalidSelection);
        }
        let row_bytes = entries[index]
            .copy_text_bytes()
            .ok_or(CopyError::SizeOverflow)?;
        total = add_row_bytes(total, row_bytes, separator, limit)?;
        separator = true;
    }
    let mut text = String::new();
    if total == 0 {
        return Ok(text);
    }
    reserve(&mut text, total)?;
    for index in indices {
        if cancelled() {
            return Err(CopyError::Cancelled);
        }
        // Standard selection iterators yield the same rows when cloned. Also
        // guard a differing iterator against stale indices or output growth.
        if index >= entries.len() {
            return Err(CopyError::InvalidSelection);
        }
        let row_bytes = entries[index]
            .copy_text_bytes()
            .ok_or(CopyError::SizeOverflow)?;
        let next = add_row_bytes(text.len(), row_bytes, !text.is_empty(), limit)?;
        if next > total {
            return Err(CopyError::InvalidSelection);
        }
        if !text.is_empty() {
            text.push('\n');
        }
        entries[index].append_copy_text(&mut text);
        debug_assert_eq!(text.len(), next);
    }
    if text.len() != total {
        return Err(CopyError::InvalidSelection);
    }
    Ok(text)
}

/// Keep only small, bounded copies on the UI thread. This is a cheap prefix
/// check, not the final preflight; the worker still validates the full request.
pub fn requires_background(
    entries: &LogStore,
    indices: impl Iterator<Item = usize>,
) -> Result<bool, CopyError> {
    let mut total = 0;
    for (count, index) in indices.enumerate() {
        if index >= entries.len() {
            return Err(CopyError::InvalidSelection);
        }
        let bytes = entries[index]
            .copy_text_bytes()
            .ok_or(CopyError::SizeOverflow)?;
        total = add_row_bytes(total, bytes, count > 0, MAX_COPY_BYTES)?;
        if count >= 256 || total > 64 * 1024 {
            return Ok(true);
        }
    }
    Ok(false)
}

struct Request {
    generation: u64,
    entries: Arc<LogStore>,
    indices: Vec<usize>,
}
pub struct Prepared {
    pub generation: u64,
    pub result: Result<String, CopyError>,
}
#[derive(Default)]
struct Pending {
    request: Option<Request>,
    closed: bool,
}
#[derive(Default)]
struct Mailbox {
    pending: Mutex<Pending>,
    ready: Condvar,
    revision: AtomicU64,
    result: Mutex<Option<Prepared>>,
}
pub struct CopyWorker(Arc<Mailbox>);
impl CopyWorker {
    pub fn new(wake: WakeSignal) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let worker = Arc::clone(&mailbox);
        std::thread::Builder::new()
            .name("copy-prepare".into())
            .spawn(move || {
                loop {
                    let request = {
                        let mut pending = worker.pending.lock().unwrap();
                        while pending.request.is_none() && !pending.closed {
                            pending = worker.ready.wait(pending).unwrap();
                        }
                        if pending.closed {
                            break;
                        }
                        pending.request.take().unwrap()
                    };
                    let cancelled =
                        || worker.revision.load(Ordering::Acquire) != request.generation;
                    let result = build_cancellable(
                        &request.entries,
                        request.indices.iter().copied(),
                        MAX_COPY_BYTES,
                        |text, bytes| {
                            text.try_reserve_exact(bytes)
                                .map_err(|_| CopyError::AllocationFailed)
                        },
                        cancelled,
                    );
                    if !cancelled() {
                        *worker.result.lock().unwrap() = Some(Prepared {
                            generation: request.generation,
                            result,
                        });
                        wake.notify();
                    }
                }
            })
            .expect("failed to start copy preparation worker");
        Self(mailbox)
    }
    pub fn submit(&self, entries: Arc<LogStore>, indices: Vec<usize>) -> u64 {
        let generation = self
            .0
            .revision
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.0.result.lock().unwrap().take();
        self.0.pending.lock().unwrap().request = Some(Request {
            generation,
            entries,
            indices,
        });
        self.0.ready.notify_one();
        generation
    }
    pub fn cancel(&self) {
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        self.0.pending.lock().unwrap().request = None;
        self.0.result.lock().unwrap().take();
    }
    pub fn take(&self) -> Option<Prepared> {
        self.0.result.lock().unwrap().take()
    }
}
impl Drop for CopyWorker {
    fn drop(&mut self) {
        self.cancel();
        self.0.pending.lock().unwrap().closed = true;
        self.0.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, LogEntry};
    use std::{cell::Cell, sync::Arc};

    fn entries() -> LogStore {
        let mut entries = LogStore::default();
        for (pid, process, message) in [
            (7, None, "first\ncontinuation"),
            (u32::MAX, Some("请求🦀worker"), "a\r\nb\n\0🦀"),
            (0, Some("a very long process identity beyond padding"), ""),
        ] {
            let mut entry = LogEntry::new("12:34", pid, pid, "标签", Level::Info, message);
            entry.process = process.map(Arc::from);
            entries.push(entry);
        }
        entries
    }

    #[test]
    fn exact_limit_preserves_full_formatter_output_and_filtered_order() {
        let entries = entries();
        for rows in [vec![], vec![1], vec![0, 2], vec![0, 1, 2]] {
            let expected = rows
                .iter()
                .map(|&index| entries[index].copy_text())
                .collect::<Vec<_>>()
                .join("\n");
            let actual = build_copy(&entries, rows.iter().copied(), expected.len()).unwrap();
            assert_eq!(actual, expected);
            assert!(actual.capacity() >= actual.len());
            assert_eq!(
                build_copy(&entries, rows.iter().copied(), expected.len() + 1).unwrap(),
                expected
            );
            if !expected.is_empty() {
                assert!(matches!(
                    build_copy(&entries, rows.iter().copied(), expected.len() - 1),
                    Err(CopyError::TooLarge { .. })
                ));
            }
        }
    }

    #[test]
    fn oversized_invalid_and_empty_requests_do_not_reserve_output() {
        let entries = entries();
        let calls = Cell::new(0);
        let reserve = |_: &mut String, _: usize| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let limit = entries[0].copy_text_bytes().unwrap() - 1;
        assert!(matches!(
            build_copy_with_reserve(&entries, [0, 1].into_iter(), limit, reserve),
            Err(CopyError::TooLarge { .. })
        ));
        assert_eq!(
            build_copy_with_reserve(&entries, [entries.len()].into_iter(), usize::MAX, reserve),
            Err(CopyError::InvalidSelection)
        );
        assert_eq!(
            build_copy_with_reserve(&entries, std::iter::empty(), 0, reserve),
            Ok(String::new())
        );
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn reservation_is_once_for_exact_bytes_and_failure_returns_no_output() {
        let entries = entries();
        let expected =
            entries[0].copy_text_bytes().unwrap() + 1 + entries[2].copy_text_bytes().unwrap();
        let calls = Cell::new(0);
        let actual = build_copy_with_reserve(
            &entries,
            [0, 2].into_iter(),
            MAX_COPY_BYTES,
            |text, bytes| {
                calls.set(calls.get() + 1);
                assert!(text.is_empty());
                assert_eq!(bytes, expected);
                text.try_reserve_exact(bytes)
                    .map_err(|_| CopyError::AllocationFailed)
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(actual.len(), expected);
        assert_eq!(
            build_copy_with_reserve(&entries, [0].into_iter(), MAX_COPY_BYTES, |_, _| {
                Err(CopyError::AllocationFailed)
            }),
            Err(CopyError::AllocationFailed)
        );
    }

    #[test]
    fn byte_arithmetic_checks_separator_overflow_and_exact_boundary() {
        assert_eq!(add_row_bytes(0, 10, false, 10), Ok(10));
        assert_eq!(add_row_bytes(10, 10, true, 21), Ok(21));
        assert_eq!(
            add_row_bytes(10, 10, true, 20),
            Err(CopyError::TooLarge {
                limit_bytes: 20,
                minimum_bytes: 21
            })
        );
        assert_eq!(
            add_row_bytes(usize::MAX, 0, true, usize::MAX),
            Err(CopyError::SizeOverflow)
        );
        assert_eq!(
            add_row_bytes(usize::MAX - 1, 2, false, usize::MAX),
            Err(CopyError::SizeOverflow)
        );
    }

    #[test]
    fn cancellation_during_formatting_returns_no_partial_result() {
        let entries = entries();
        let calls = Cell::new(0);
        let result = build_cancellable(
            &entries,
            0..3,
            MAX_COPY_BYTES,
            |text, bytes| {
                text.reserve(bytes);
                Ok(())
            },
            || {
                calls.set(calls.get() + 1);
                calls.get() == 5
            },
        );
        assert_eq!(result, Err(CopyError::Cancelled));
        assert_eq!(
            calls.get(),
            5,
            "cancel after preflight and one formatted row"
        );
    }

    #[test]
    fn worker_supersedes_old_generation_and_releases_snapshots() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = CopyWorker::new(WakeSignal::new(move || {
            let _ = tx.send(());
        }));
        let entries = Arc::new(entries());
        let old = worker.submit(Arc::clone(&entries), vec![0; 200_000]);
        worker.cancel();
        let current = worker.submit(Arc::clone(&entries), vec![2]);
        assert_ne!(old, current);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if let Some(prepared) = worker.take()
                && prepared.generation == current
            {
                assert_eq!(prepared.result.unwrap(), entries[2].copy_text());
                break;
            }
        }
        while Arc::strong_count(&entries) != 1 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
}
