const HISTORY_LIMIT: usize = 500;

pub fn record(entries: &mut Vec<String>, query: &str) {
    if query.trim().is_empty() {
        return;
    }
    entries.retain(|entry| entry != query);
    entries.insert(0, query.to_owned());
    entries.truncate(HISTORY_LIMIT);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_exact_text_and_moves_reused_queries_to_front() {
        let mut entries = Vec::new();
        for query in ["", "  ", " timeout|失败 ", "second", " timeout|失败 "] {
            record(&mut entries, query);
        }
        assert_eq!(entries, [" timeout|失败 ", "second"]);
    }
    #[test]
    fn discards_oldest_entries_at_limit() {
        let mut entries = Vec::new();
        for index in 0..600 {
            record(&mut entries, &index.to_string());
        }
        assert_eq!(entries.len(), HISTORY_LIMIT);
        assert_eq!(entries[0], "599");
        assert_eq!(entries[HISTORY_LIMIT - 1], "100");
    }
}
