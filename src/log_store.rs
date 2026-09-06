use std::{collections::VecDeque, ops::Index, sync::Arc};

use crate::model::LogEntry;

const CHUNK_SIZE: usize = 1024;
const CHUNK_BYTES: usize = CHUNK_SIZE * std::mem::size_of::<LogEntry>() + 64;

struct Chunk(VecDeque<LogEntry>);

impl Clone for Chunk {
    fn clone(&self) -> Self {
        let mut entries = VecDeque::with_capacity(CHUNK_SIZE);
        entries.extend(self.0.iter().cloned());
        Self(entries)
    }
}

/// Filter snapshots share immutable chunks. Capture only copies a changed chunk,
/// rather than all retained entries, when a snapshot is still in use.
#[derive(Clone, Default)]
pub struct LogStore {
    chunks: VecDeque<Arc<Chunk>>,
    offset: usize,
    len: usize,
    bytes: usize,
}

impl LogStore {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn memory_bytes(&self) -> usize {
        self.bytes
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.chunks.back().is_none_or(|chunk| {
            chunk.0.len()
                + if self.chunks.len() == 1 {
                    self.offset
                } else {
                    0
                }
                == CHUNK_SIZE
        }) {
            self.chunks
                .push_back(Arc::new(Chunk(VecDeque::with_capacity(CHUNK_SIZE))));
            self.bytes += CHUNK_BYTES;
        }
        self.bytes += entry.payload_bytes();
        Arc::make_mut(self.chunks.back_mut().unwrap())
            .0
            .push_back(entry);
        self.len += 1;
    }

    pub fn iter(&self) -> impl Iterator<Item = &LogEntry> {
        self.chunks.iter().flat_map(|chunk| chunk.0.iter())
    }

    pub fn set_process(&mut self, index: usize, process: Arc<str>) {
        if index >= self.len {
            return;
        }
        let absolute = self.offset + index;
        let chunk = absolute / CHUNK_SIZE;
        let position = if chunk == 0 {
            index
        } else {
            absolute % CHUNK_SIZE
        };
        let entry = &mut Arc::make_mut(&mut self.chunks[chunk]).0[position];
        self.bytes -= entry.payload_bytes();
        entry.process = Some(process);
        self.bytes += entry.payload_bytes();
    }

    pub fn drain_front(&mut self, count: usize) {
        assert!(count <= self.len);
        for _ in 0..count {
            let chunk = Arc::make_mut(self.chunks.front_mut().unwrap());
            self.bytes -= chunk.0.pop_front().unwrap().payload_bytes();
            self.offset += 1;
            self.len -= 1;
            if chunk.0.is_empty() {
                self.chunks.pop_front();
                self.bytes -= CHUNK_BYTES;
                self.offset = 0;
            }
        }
    }

    pub fn trim_to_bytes(&mut self, target: usize) -> usize {
        let mut removed = 0;
        while self.bytes > target && !self.is_empty() {
            self.drain_front(1);
            removed += 1;
        }
        removed
    }
}

impl Index<usize> for LogStore {
    type Output = LogEntry;
    fn index(&self, index: usize) -> &Self::Output {
        assert!(index < self.len);
        let absolute = self.offset + index;
        let chunk = absolute / CHUNK_SIZE;
        &self.chunks[chunk].0[if chunk == 0 {
            index
        } else {
            absolute % CHUNK_SIZE
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_budget_evicts_large_logs_before_small_logs() {
        let mut small = LogStore::default();
        let mut large = LogStore::default();
        for _ in 0..100 {
            small.push(LogEntry::marker("tiny"));
            large.push(LogEntry::marker("x".repeat(32 * 1024)));
        }
        let budget = 512 * 1024;
        assert_eq!(small.trim_to_bytes(budget), 0);
        assert!(large.trim_to_bytes(budget) > 0);
        assert!(large.memory_bytes() <= budget);
        assert!(!large.is_empty());
        let snapshot = large.clone();
        large.drain_front(large.len());
        assert_eq!(large.memory_bytes(), 0);
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn partial_chunk_eviction_and_append_keep_order_and_accounting() {
        let mut store = LogStore::default();
        let mut expected = VecDeque::new();
        for index in 0..10_000 {
            store.push(LogEntry::marker(index.to_string()));
            expected.push_back(index.to_string());
            if index % 11 == 0 && !expected.is_empty() {
                store.drain_front(1);
                expected.pop_front();
            }
            if store.memory_bytes() > 400_000 {
                let removed = store.trim_to_bytes(300_000);
                expected.drain(..removed);
            }
            assert_eq!(store.len(), expected.len());
            if let Some(first) = expected.front() {
                assert_eq!(store[0].message.as_ref(), first);
            }
            if let Some(last) = expected.back() {
                assert_eq!(store[store.len() - 1].message.as_ref(), last);
            }
        }
        let payload: usize = store.iter().map(LogEntry::payload_bytes).sum();
        assert_eq!(
            store.memory_bytes(),
            payload + store.chunks.len() * CHUNK_BYTES
        );
    }

    #[test]
    fn snapshots_survive_append_resolution_and_repeated_trimming() {
        let mut store = LogStore::default();
        for index in 0..2500 {
            store.push(LogEntry::marker(index.to_string()));
        }
        let snapshot = store.clone();
        store.drain_front(1500);
        store.set_process(0, Arc::from("resolved"));
        for index in 2500..4000 {
            store.push(LogEntry::marker(index.to_string()));
        }
        assert_eq!(store.len(), 2500);
        assert_eq!(store[0].message.as_ref(), "1500");
        assert_eq!(store[2499].message.as_ref(), "3999");
        assert!(snapshot[1500].process.is_none());
        assert_eq!(snapshot.len(), 2500);
        assert_eq!(store.iter().count(), store.len());
        store.drain_front(700);
        assert_eq!(store[0].message.as_ref(), "2200");
        store.drain_front(store.len());
        assert!(store.is_empty());
        store.push(LogEntry::marker("new"));
        assert_eq!(store[0].message.as_ref(), "new");
        store.set_process(1, Arc::from("ignored"));
    }
}
