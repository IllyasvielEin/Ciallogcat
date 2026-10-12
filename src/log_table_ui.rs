//! Small table-specific layout state and semantic cell wrappers.
use eframe::egui;

pub(super) const HEADERS: [&str; 6] = ["TIME", "LV", "PID / TID", "PROCESS", "TAG", "MESSAGE"];

#[derive(Default)]
pub(super) struct LogColumns {
    compact: Option<bool>,
    // Explicit drags override only that column for this application session.
    manual: [Option<f32>; 2],
    // egui applies divider motion after painting; capture its width next frame.
    dragging: [bool; 2],
}
impl LogColumns {
    pub(super) fn prepare(&mut self, compact: bool) -> bool {
        if self.dragging.iter().any(|dragging| *dragging) {
            return false;
        }
        let changed = self.compact.is_some_and(|old| old != compact);
        self.compact = Some(compact);
        changed
    }
    pub(super) fn widths(&self, compact: bool) -> [f32; 2] {
        let defaults = if compact {
            [140.0, 110.0]
        } else {
            [210.0, 165.0]
        };
        std::array::from_fn(|i| self.manual[i].unwrap_or(defaults[i]))
    }
    pub(super) fn finish(&mut self, ctx: &egui::Context, table_id: egui::Id, widths: [f32; 2]) {
        for (i, width) in widths.into_iter().enumerate() {
            if self.dragging[i] {
                self.manual[i] = Some(width);
            }
            self.dragging[i] = ctx
                .read_response(table_id.with("resize_column").with(i + 3))
                .is_some_and(|response| response.dragged());
        }
    }
}

/// Real focusable row widget. The first cell supplies the row y range, but
/// its narrow horizontal clip must not limit row hit-testing or focus bounds.
pub(super) fn row_response(
    ui: &mut egui::Ui,
    identity: egui::Id,
    table: egui::Rect,
    clip: egui::Rect,
) -> egui::Response {
    let rect = egui::Rect::from_x_y_ranges(table.x_range(), ui.max_rect().y_range());
    let clip = egui::Rect::from_x_y_ranges(clip.x_range(), ui.clip_rect().y_range());
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id(identity)
            .max_rect(rect)
            .sense(egui::Sense::click()),
    );
    row.set_clip_rect(clip);
    row.set_min_size(rect.size());
    row.response()
}

pub(super) fn cell(
    ui: &mut egui::Ui,
    row_id: egui::Id,
    row: usize,
    column: usize,
    value: &str,
    header: Option<egui::Id>,
    paint: impl FnOnce(&mut egui::Ui),
) -> egui::Id {
    if column != 0 {
        // Field cells are hover-only; the stable full-width row owns pointer
        // actions as well as focus. Its semantic children are not six buttons.
        ui.ctx()
            .accesskit_node_builder(ui.unique_id(), |node| node.set_hidden());
    }
    let rect = ui.max_rect();
    let inner = ui.scope_builder(
        egui::UiBuilder::new()
            .id(row_id.with(("field", column)))
            .accessibility_parent(row_id),
        |ui| {
            let id = ui.unique_id();
            ui.ctx().accesskit_node_builder(id, |node| {
                node.set_role(if header.is_some() {
                    egui::accesskit::Role::Cell
                } else {
                    egui::accesskit::Role::ColumnHeader
                });
                node.set_row_index(row);
                node.set_column_index(column);
                node.set_label(value);
                if let Some(header) = header {
                    node.set_described_by(vec![header.accesskit_id()]);
                }
            });
            paint(ui);
            id
        },
    );
    ui.ctx().accesskit_node_builder(inner.inner, |node| {
        node.set_bounds(egui::accesskit::Rect {
            x0: rect.min.x as f64,
            y0: rect.min.y as f64,
            x1: rect.max.x as f64,
            y1: rect.max.y as f64,
        });
    });
    inner.inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_defaults_return_without_overwriting_individual_drags() {
        let mut columns = LogColumns::default();
        assert!(!columns.prepare(false));
        assert_eq!(columns.widths(false), [210.0, 165.0]);
        assert!(columns.prepare(true));
        assert_eq!(columns.widths(true), [140.0, 110.0]);
        assert!(!columns.prepare(true));
        assert!(columns.prepare(false));
        assert_eq!(columns.widths(false), [210.0, 165.0]);
        columns.manual[0] = Some(188.0);
        assert!(columns.prepare(true));
        assert_eq!(columns.widths(true), [188.0, 110.0]);
        assert!(columns.prepare(false));
        assert_eq!(columns.widths(false), [188.0, 165.0]);
    }

    #[test]
    fn breakpoint_reset_waits_for_pending_actual_divider_drag() {
        let mut columns = LogColumns::default();
        columns.prepare(false);
        columns.dragging[1] = true;
        assert!(!columns.prepare(true));
        assert_eq!(columns.compact, Some(false));
        columns.finish(
            &egui::Context::default(),
            egui::Id::new("test"),
            [210.0, 135.0],
        );
        assert_eq!(columns.manual, [None, Some(135.0)]);
        assert!(columns.prepare(true));
        assert_eq!(columns.widths(true), [140.0, 135.0]);
    }
}
