use std::{collections::VecDeque, ops::Index, sync::Arc};

use crate::model::LogEntry;

const CHUNK_SIZE: usize = 1024;
const CHUNK_BYTES: usize = CHUNK_SIZE * std::mem::size_of::<LogEntry>() + 64;

struct Chunk {
    entries: VecDeque<LogEntry>,
    payload_bytes: usize,
    unresolved: usize,
    incidents: usize,
}

#[cfg(test)]
thread_local! {
    static CHUNK_CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Clone for Chunk {
    fn clone(&self) -> Self {
        let mut entries = VecDeque::with_capacity(CHUNK_SIZE);
        entries.extend(self.entries.iter().cloned());
        #[cfg(test)]
        CHUNK_CLONES.with(|count| count.set(count.get() + 1));
        Self {
            entries,
            payload_bytes: self.payload_bytes,
            unresolved: self.unresolved,
            incidents: self.incidents,
        }
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
    unresolved: usize,
    incidents: usize,
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

    pub fn unresolved_count(&self) -> usize {
        self.unresolved
    }

    pub fn incident_count(&self) -> usize {
        self.incidents
    }

    /// Searches only chunks containing a known signature, then wraps once.
    /// It includes retained rows hidden by the current UI filter.
    pub fn next_incident(&self, after: Option<usize>) -> Option<usize> {
        if self.incidents == 0 {
            return None;
        }
        let start = after.map_or(0, |index| index.saturating_add(1).min(self.len));
        let find = |range: std::ops::Range<usize>| {
            let mut base = 0;
            for chunk in &self.chunks {
                let end = base + chunk.entries.len();
                if base >= range.end {
                    break;
                }
                if chunk.incidents > 0 && end > range.start {
                    let from = range.start.saturating_sub(base);
                    let to = range.end.min(end) - base;
                    for index in from..to {
                        if crate::incident::classify(&chunk.entries[index]).is_some() {
                            return Some(base + index);
                        }
                    }
                }
                base = end;
            }
            None
        };
        find(start..self.len).or_else(|| find(0..start))
    }

    fn needs_chunk(&self) -> bool {
        self.chunks.back().is_none_or(|chunk| {
            chunk.entries.len()
                + if self.chunks.len() == 1 {
                    self.offset
                } else {
                    0
                }
                == CHUNK_SIZE
        })
    }

    /// Includes new record storage capacity, in the same units as memory_bytes.
    /// Shared snapshots and allocator overhead remain outside this estimate.
    pub fn next_push_memory_bytes(&self, entry: &LogEntry) -> usize {
        entry
            .payload_bytes()
            .saturating_add(if self.needs_chunk() { CHUNK_BYTES } else { 0 })
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.needs_chunk() {
            self.chunks.push_back(Arc::new(Chunk {
                entries: VecDeque::with_capacity(CHUNK_SIZE),
                payload_bytes: 0,
                unresolved: 0,
                incidents: 0,
            }));
            self.bytes += CHUNK_BYTES;
        }
        let payload = entry.payload_bytes();
        let unresolved = usize::from(entry.is_unresolved_process());
        let incident = usize::from(crate::incident::classify(&entry).is_some());
        self.bytes += payload;
        self.unresolved += unresolved;
        self.incidents += incident;
        let chunk = Arc::make_mut(self.chunks.back_mut().unwrap());
        chunk.payload_bytes += payload;
        chunk.unresolved += unresolved;
        chunk.incidents += incident;
        chunk.entries.push_back(entry);
        self.len += 1;
    }

    pub fn iter(&self) -> impl Iterator<Item = &LogEntry> {
        self.chunks.iter().flat_map(|chunk| chunk.entries.iter())
    }

    pub fn set_process(&mut self, index: usize, process: Arc<str>) {
        if index >= self.len || self[index].kind != crate::model::LogEntryKind::Log {
            return;
        }
        let absolute = self.offset + index;
        let chunk = absolute / CHUNK_SIZE;
        let position = if chunk == 0 {
            index
        } else {
            absolute % CHUNK_SIZE
        };
        let chunk = Arc::make_mut(&mut self.chunks[chunk]);
        let entry = &mut chunk.entries[position];
        let before = entry.payload_bytes();
        if entry.is_unresolved_process() {
            chunk.unresolved -= 1;
            self.unresolved -= 1;
        }
        entry.process = Some(process);
        entry.process_source = None;
        let after = entry.payload_bytes();
        self.bytes = self.bytes - before + after;
        chunk.payload_bytes = chunk.payload_bytes - before + after;
    }

    fn drop_front_chunk(&mut self) -> usize {
        let chunk = self.chunks.pop_front().unwrap();
        let removed = chunk.entries.len();
        self.bytes -= CHUNK_BYTES + chunk.payload_bytes;
        self.unresolved -= chunk.unresolved;
        self.incidents -= chunk.incidents;
        self.len -= removed;
        self.offset = 0;
        // In particular, never make_mut a chunk that is about to be discarded.
        removed
    }

    pub fn drain_front(&mut self, mut count: usize) {
        assert!(count <= self.len);
        while count > 0 {
            if count >= self.chunks.front().unwrap().entries.len() {
                count -= self.drop_front_chunk();
                continue;
            }
            let chunk = Arc::make_mut(self.chunks.front_mut().unwrap());
            for _ in 0..count {
                let entry = chunk.entries.pop_front().unwrap();
                let payload = entry.payload_bytes();
                let unresolved = usize::from(entry.is_unresolved_process());
                let incident = usize::from(crate::incident::classify(&entry).is_some());
                chunk.payload_bytes -= payload;
                chunk.unresolved -= unresolved;
                chunk.incidents -= incident;
                self.bytes -= payload;
                self.unresolved -= unresolved;
                self.incidents -= incident;
            }
            self.offset += count;
            self.len -= count;
            break;
        }
    }

    pub fn trim_to_bytes(&mut self, target: usize) -> usize {
        let before = self.len;
        while self.bytes > target && !self.is_empty() {
            let front = self.chunks.front().unwrap();
            if self.bytes - (CHUNK_BYTES + front.payload_bytes) >= target {
                self.drop_front_chunk();
            } else {
                // Only the final boundary chunk can require copy-on-write.
                let mut count = 0;
                let mut remaining = self.bytes;
                for entry in &front.entries {
                    count += 1;
                    remaining -= entry.payload_bytes();
                    if remaining <= target {
                        break;
                    }
                }
                self.drain_front(count);
            }
        }
        before - self.len
    }
}

impl Index<usize> for LogStore {
    type Output = LogEntry;
    fn index(&self, index: usize) -> &Self::Output {
        assert!(index < self.len);
        let absolute = self.offset + index;
        let chunk = absolute / CHUNK_SIZE;
        &self.chunks[chunk].entries[if chunk == 0 {
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
    fn incident_navigation_wraps_and_survives_partial_and_whole_chunk_eviction() {
        let mut store = LogStore::default();
        for index in 0..2500 {
            store.push(if [5, 1050, 2300].contains(&index) {
                LogEntry::new(
                    "",
                    1,
                    1,
                    "AndroidRuntime",
                    crate::model::Level::Error,
                    "FATAL EXCEPTION: main",
                )
            } else {
                LogEntry::marker("ordinary")
            });
        }
        let snapshot = store.clone();
        assert_eq!(store.incident_count(), 3);
        assert_eq!(store.next_incident(None), Some(5));
        assert_eq!(store.next_incident(Some(1050)), Some(2300));
        assert_eq!(store.next_incident(Some(2300)), Some(5));
        store.drain_front(1040);
        assert_eq!(store.incident_count(), 2);
        assert_eq!(store.next_incident(None), Some(10));
        assert_eq!(store.next_incident(Some(10)), Some(1260));
        store.drain_front(11);
        assert_eq!(store.incident_count(), 1);
        store.drain_front(store.len());
        assert_eq!(store.incident_count(), 0);
        assert_eq!(store.next_incident(None), None);
        assert_eq!(snapshot.incident_count(), 3);
        assert_eq!(snapshot.next_incident(Some(1050)), Some(2300));
    }

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
            store.push(LogEntry::new(
                "",
                1,
                1,
                "Tag",
                crate::model::Level::Info,
                index.to_string(),
            ));
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
    #[test]
    fn trimming_shared_whole_chunks_never_clones_discarded_rows() {
        let mut store = LogStore::default();
        for index in 0..4096 {
            store.push(LogEntry::marker(index.to_string()));
        }
        let snapshot = store.clone();
        let first_two_bytes: usize = store
            .chunks
            .iter()
            .take(2)
            .map(|chunk| CHUNK_BYTES + chunk.payload_bytes)
            .sum();
        let before = CHUNK_CLONES.with(|count| count.get());
        assert_eq!(
            store.trim_to_bytes(store.memory_bytes() - first_two_bytes),
            2048
        );
        assert_eq!(CHUNK_CLONES.with(|count| count.get()), before);
        assert_eq!(store[0].message.as_ref(), "2048");
        assert_eq!(snapshot[0].message.as_ref(), "0");
        let before = CHUNK_CLONES.with(|count| count.get());
        store.trim_to_bytes(store.memory_bytes() - 1000);
        assert_eq!(CHUNK_CLONES.with(|count| count.get()), before + 1);
    }

    #[test]
    fn cached_chunk_counts_match_full_recount_after_resolution_and_trim() {
        let mut store = LogStore::default();
        for _ in 0..2500 {
            store.push(LogEntry::new(
                "",
                1,
                1,
                "Tag",
                crate::model::Level::Info,
                "x",
            ));
        }
        let snapshot = store.clone();
        for index in (0..2500).step_by(3) {
            store.set_process(index, Arc::from("resolved"));
        }
        store.drain_front(1041);
        store.trim_to_bytes(store.memory_bytes() - 15000);
        assert_eq!(
            store.unresolved_count(),
            store
                .iter()
                .filter(|entry| entry.is_unresolved_process())
                .count()
        );
        assert_eq!(
            store.memory_bytes(),
            store.iter().map(LogEntry::payload_bytes).sum::<usize>()
                + store.chunks.len() * CHUNK_BYTES
        );
        assert_eq!(snapshot.unresolved_count(), 2500);
    }

    #[test]
    fn structural_and_unparsed_records_never_affect_unknown_process_counts() {
        let mut store = LogStore::default();
        store.push(LogEntry::marker("--------- beginning of main"));
        store.push(LogEntry::unparsed("broken record"));
        store.push(LogEntry::new(
            "",
            42,
            42,
            "Tag",
            crate::model::Level::Error,
            "crash",
        ));
        assert_eq!(store.unresolved_count(), 1);
        store.set_process(0, Arc::from("must not assign structural rows"));
        store.set_process(1, Arc::from("must not assign undecodable rows"));
        assert!(store[0].process.is_none() && store[1].process.is_none());
        assert_eq!(store.unresolved_count(), 1);
        store.drain_front(2);
        assert_eq!(store.unresolved_count(), 1);
        store.set_process(0, Arc::from("sample"));
        assert_eq!(store.unresolved_count(), 0);
        store.drain_front(1);
        assert_eq!(store.unresolved_count(), 0);
    }
}
