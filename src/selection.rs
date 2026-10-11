use crate::log_store::LogStore;
use std::collections::BTreeSet;

/// Indices refer to the retained store; ranges follow only filtered rows.
#[derive(Default)]
pub struct RowSelection {
    rows: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl RowSelection {
    pub fn contains(&self, index: usize) -> bool {
        self.rows.contains(&index)
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub fn clear(&mut self) {
        self.rows.clear();
        self.anchor = None;
    }
    pub fn retain(&mut self, matches: &[usize]) {
        self.rows
            .retain(|index| matches.binary_search(index).is_ok());
        if self
            .anchor
            .is_some_and(|index| matches.binary_search(&index).is_err())
        {
            self.anchor = None;
        }
    }
    pub fn select_all(&mut self, matches: &[usize]) {
        self.rows = matches.iter().copied().collect();
        self.anchor = matches.first().copied();
    }
    pub fn select(&mut self, matches: &[usize], index: usize, extend: bool, toggle: bool) {
        let Ok(end) = matches.binary_search(&index) else {
            return;
        };
        if extend {
            let start = self
                .anchor
                .and_then(|anchor| matches.binary_search(&anchor).ok())
                .unwrap_or(end);
            if !toggle {
                self.rows.clear();
            }
            self.rows
                .extend(matches[start.min(end)..=start.max(end)].iter().copied());
            self.anchor.get_or_insert(index);
        } else {
            if !toggle {
                self.rows.clear();
            }
            if !toggle || !self.rows.remove(&index) {
                self.rows.insert(index);
            }
            self.anchor = Some(index);
        }
    }
    pub fn copy_text(&self, entries: &LogStore) -> String {
        let mut text = String::new();
        for &index in &self.rows {
            if !text.is_empty() {
                text.push('\n');
            }
            entries[index].append_copy_text(&mut text);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LogEntry;
    #[test]
    fn range_uses_filtered_order_and_keeps_anchor_when_reversed() {
        let mut selection = RowSelection::default();
        let matches = [1, 4, 8, 12];
        selection.select(&matches, 8, false, false);
        selection.select(&matches, 1, true, false);
        assert_eq!(selection.rows, BTreeSet::from([1, 4, 8]));
        selection.select(&matches, 12, true, false);
        assert_eq!(selection.rows, BTreeSet::from([8, 12]));
    }
    #[test]
    fn toggles_and_filter_changes_never_copy_hidden_rows() {
        let mut entries = LogStore::default();
        for message in ["first\ncontinuation", "hidden", "third", "last"] {
            entries.push(LogEntry::marker(message));
        }
        let mut selection = RowSelection::default();
        selection.select(&[0, 2, 3], 3, false, false);
        selection.select(&[0, 2, 3], 0, false, true);
        assert_eq!(
            selection.copy_text(&entries),
            format!("{}\n{}", entries[0].copy_text(), entries[3].copy_text())
        );
        selection.select(&[0, 2, 3], 3, false, true);
        assert_eq!(selection.copy_text(&entries), entries[0].copy_text());
        selection.select_all(&[0, 2, 3]);
        selection.retain(&[2]);
        assert_eq!(selection.copy_text(&entries), entries[2].copy_text());
        assert_eq!(selection.anchor, None);
        selection.clear();
        assert!(selection.is_empty());
        assert!(selection.copy_text(&entries).is_empty());
    }
    #[test]
    fn direct_copy_format_preserves_original_fields_and_multiline_text() {
        let mut entries = LogStore::default();
        let mut expected = Vec::new();
        for (pid, process, message) in [
            (7, None, "first\ncontinuation"),
            (u32::MAX, Some("请求🦀worker"), "a\r\nb\n"),
            (0, Some("a very long process identity beyond padding"), ""),
        ] {
            let mut entry = crate::model::LogEntry::new(
                "12:34",
                pid,
                pid,
                "标签",
                crate::model::Level::Info,
                message,
            );
            entry.process = process.map(std::sync::Arc::from);
            let original = format!(
                "{} {:>5} {:>5} {} {:<24} {}: {}",
                entry.time,
                entry.pid,
                entry.tid,
                entry.level.short(),
                entry.process_name(),
                entry.tag,
                entry.message
            );
            assert_eq!(entry.copy_text(), original);
            expected.push(original);
            entries.push(entry);
        }
        let mut selection = RowSelection::default();
        selection.select_all(&[0, 1, 2]);
        assert_eq!(selection.copy_text(&entries), expected.join("\n"));
    }
}
