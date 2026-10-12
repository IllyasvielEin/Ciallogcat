//! Bounded full-record clipboard serialization. Classification readiness belongs
//! to the app; this helper validates indices and sizes before allocating output.

use std::fmt;

use crate::log_store::LogStore;

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
    let mut total = 0;
    let mut separator = false;
    for index in indices.clone() {
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
}
