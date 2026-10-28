use super::*;
use reading::{ContextView, RecordRef};

impl CiallogcatApp {
    pub(super) fn jump_to_incident(&mut self) {
        if self.filter_pending || self.filter_dirty_since.is_some() {
            return;
        }
        let Some(index) = self.entries.next_incident(self.selected) else {
            self.copy_notice =
                Some("保留日志中没有匹配已知崩溃 / ANR 签名的记录；这不证明没有故障。".into());
            return;
        };
        self.leave_context();
        self.show_context(index);
        self.show_details = true;
    }

    pub(super) fn evidence_reference(&self, index: usize) -> Option<(String, String, String)> {
        if index >= self.entries.len() {
            return None;
        }
        let entry = &self.entries[index];
        if let Some(identity) = entry.provenance {
            return Some((
                format!("seq #{} · segment {}", identity.seq, identity.segment),
                format!("capture_session\nsession_id: {}\nseq: {}\nsegment: {}\nattempt: {}\nobserved_unix_ms: {}\n点击复制 session_id / seq 引用", identity.session_id, identity.seq, identity.segment, identity.attempt, identity.observed_unix_ms.map_or_else(|| "unknown".into(), |value| value.to_string())),
                serde_json::json!({"identity_scope":"capture_session","session_id":identity.session_id,"seq":identity.seq}).to_string(),
            ));
        }
        let document = self.files.document.as_ref()?;
        let metadata = document.record_metadata.get(index)?;
        Some((
            format!("artifact-local seq #{}", metadata.seq),
            format!("artifact_local\nartifact_id: {}\nseq: {}\n无来源会话身份；此引用只属于当前文档。点击复制。", document.header.artifact_id, metadata.seq),
            serde_json::json!({"identity_scope":"artifact_local","artifact_id":document.header.artifact_id,"seq":metadata.seq}).to_string(),
        ))
    }

    fn record_ref(&self, index: usize) -> RecordRef {
        RecordRef {
            epoch: self.log_epoch,
            absolute: self.dropped_entries + index,
            identity: self.entries[index].provenance,
        }
    }

    fn resolve_ref(&self, reference: RecordRef) -> Option<usize> {
        if reference.epoch != self.log_epoch {
            return None;
        }
        let index = reference.absolute.checked_sub(self.dropped_entries)?;
        (index < self.entries.len() && self.entries[index].provenance == reference.identity)
            .then_some(index)
    }

    pub(super) fn reading_rows(&self) -> &[usize] {
        self.context_view
            .as_ref()
            .map_or(&self.matches, |context| &context.rows)
    }

    pub(super) fn show_context(&mut self, index: usize) {
        if index >= self.entries.len()
            || self.context_view.is_some()
            || self.filter_pending
            || self.filter_dirty_since.is_some()
        {
            return;
        }
        let origin = self.record_ref(index);
        let row = (self.table_scroll_offset / self.table_row_stride.max(1.0)).floor() as usize;
        let viewport = self.matches.get(row).map(|&index| self.record_ref(index));
        let rows =
            (index.saturating_sub(50)..(index + 51).min(self.entries.len())).collect::<Vec<_>>();
        self.selection.select(&rows, index, false, false);
        self.context_view = Some(ContextView {
            rows,
            origin,
            viewport,
            fraction: self.table_scroll_offset % self.table_row_stride.max(1.0),
            reveal_after_notice: true,
        });
        self.selected = Some(index);
        self.follow_logs = false;
        self.scroll_to_bottom = false;
        self.scroll_to_selected = true;
        self.restore_scroll_offset = None;
    }

    pub(super) fn leave_context(&mut self) {
        let Some(context) = self.context_view.take() else {
            return;
        };
        self.selection.clear();
        self.selected = self
            .resolve_ref(context.origin)
            .filter(|index| self.matches.binary_search(index).is_ok());
        if let Some(index) = self.selected {
            self.selection.select(&self.matches, index, false, false);
        } else {
            self.copy_notice = Some("原日志已过期或不再匹配；已返回当前筛选结果。".to_owned());
        }
        self.restore_scroll_offset = context
            .viewport
            .and_then(|reference| self.resolve_ref(reference))
            .and_then(|index| self.matches.binary_search(&index).ok())
            .map(|position| position as f32 * self.table_row_stride + context.fraction);
        if self.restore_scroll_offset.is_none() {
            self.scroll_to_selected = self.selected.is_some();
            if self.selected.is_none() {
                self.restore_scroll_offset = Some(0.0);
            }
        }
        self.follow_logs = false;
        self.scroll_to_bottom = false;
    }

    pub(super) fn context_notice(&mut self, root_ui: &mut egui::Ui) {
        let Some(context) = &self.context_view else {
            return;
        };
        let count = context.rows.len();
        let expired = self.resolve_ref(context.origin).is_none();
        if context.reveal_after_notice {
            // Details/context-menu can enter after this panel's position in the
            // previous frame. Reveal again with the notice's final body height.
            self.context_view.as_mut().unwrap().reveal_after_notice = false;
            self.scroll_to_selected = true;
        }
        let mut leave = false;
        egui::Panel::top("context_notice").show(root_ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                leave = ui.button("返回筛选位置").clicked();
                ui.label(format!(
                    "上下文 · {count} 条 · 暂时显示前后各 50 条，不受筛选条件隐藏"
                ));
                if expired {
                    ui.label("原日志已过期");
                }
            });
        });
        if leave {
            self.leave_context();
        }
    }

    pub(super) fn update_detail_hits(&mut self, message: &Arc<str>) {
        if self.filter_progress.is_none_or(|progress| {
            progress.generation != self.filter_generation || progress.log_epoch != self.log_epoch
        }) || self.filter_dirty_since.is_some()
            || self.active_filter.query.is_empty()
        {
            if self.detail_search.key.is_some() {
                self.detail_search.reset();
            }
            return;
        }
        let key = (
            self.active_filter.query.clone(),
            self.active_filter.regex,
            self.active_filter.case_sensitive,
        );
        let changed = self
            .detail_search
            .key
            .as_ref()
            .is_none_or(|(old_message, old_key)| {
                !Arc::ptr_eq(message, old_message) || old_key != &key
            });
        if changed {
            self.detail_search.reset();
            if let Ok(Some(matcher)) = &self.incremental_matcher {
                self.detail_search.generation = self
                    .detail_search
                    .worker
                    .submit(Arc::clone(message), matcher.clone());
                self.detail_search.key = Some((Arc::clone(message), key));
                self.detail_search.pending = true;
            }
        }
        if let Some(result) = self.detail_search.worker.take()
            && result.generation == self.detail_search.generation
        {
            self.detail_search.hits = result.hits;
            self.detail_search.capped = result.capped;
            self.detail_search.pending = false;
            self.detail_search.current = usize::MAX;
        }
    }

    pub(super) fn move_detail_hit(&mut self, backwards: bool) {
        if self.detail_search.hits.is_empty() {
            return;
        }
        self.detail_search.current = if backwards {
            if self.detail_search.current >= self.detail_search.hits.len() {
                self.detail_search.hits.len() - 1
            } else {
                self.detail_search
                    .current
                    .checked_sub(1)
                    .unwrap_or(self.detail_search.hits.len() - 1)
            }
        } else if self.detail_search.current < self.detail_search.hits.len() {
            (self.detail_search.current + 1) % self.detail_search.hits.len()
        } else {
            0
        };
        self.detail_search.jump = true;
    }
}

/// Highlight only ranges intersecting this displayed segment. Matching always
/// happens against the complete message, so anchors and cross-segment regexes
/// retain the same meaning as filtering.
pub(super) fn detail_layout(
    text: &str,
    offset: usize,
    hits: &[std::ops::Range<usize>],
    current: Option<usize>,
    color: Color32,
    dark: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let base = TextFormat {
        font_id: FontId::new(12.5, FontFamily::Monospace),
        color,
        ..Default::default()
    };
    let mut cursor = 0;
    let begin = hits.partition_point(|hit| hit.end <= offset && hit.start < offset);
    for (index, hit) in hits.iter().enumerate().skip(begin) {
        if hit.start > offset + text.len() {
            break;
        }
        let start = hit.start.saturating_sub(offset).min(text.len());
        let end = hit.end.saturating_sub(offset).min(text.len());
        if start < cursor {
            continue;
        }
        job.append(&text[cursor..start], 0.0, base.clone());
        let mut highlighted = base.clone();
        highlighted.background = if current == Some(index) {
            Color32::from_rgb(135, 92, 12)
        } else if dark {
            Color32::from_rgb(87, 70, 24)
        } else {
            Color32::from_rgb(255, 232, 159)
        };
        highlighted.color = if current == Some(index) || dark {
            Color32::from_rgb(255, 235, 175)
        } else {
            Color32::from_rgb(100, 60, 0)
        };
        job.append(&text[start..end], 0.0, highlighted);
        cursor = end;
    }
    job.append(&text[cursor..], 0.0, base);
    job
}
