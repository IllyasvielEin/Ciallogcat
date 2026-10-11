//! Real egui frames: responsive geometry and exposed row actions.
use super::ui_test_support::Harness;
use super::*;
use egui::accesskit::{Action, ActionRequest, Node, NodeId, Role, TreeId};

fn table() -> Harness {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.follow_logs = false;
    app.show_details = false;
    for i in 0..300 {
        app.append_entry(LogEntry::new(
            "10-11 13:00:00.123",
            100,
            i,
            "中文标签",
            Level::Info,
            format!("message-{i} 🦀"),
        ));
    }
    Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0))
}
fn node(h: &Harness, id: NodeId) -> &Node {
    &h.accessible_nodes
        .iter()
        .find(|(key, _)| *key == id)
        .expect("reachable node")
        .1
}
fn headers(h: &Harness) -> Vec<&Node> {
    let mut nodes: Vec<_> = h
        .accessible_nodes
        .iter()
        .filter(|(_, n)| n.role() == Role::ColumnHeader)
        .map(|(_, n)| n)
        .collect();
    nodes.sort_by_key(|n| n.column_index());
    nodes
}
fn rows(h: &Harness) -> Vec<(NodeId, usize)> {
    let mut rows: Vec<_> = h
        .accessible_nodes
        .iter()
        .filter(|(_, n)| n.role() == Role::Row && n.row_index().unwrap_or(0) > 0)
        .map(|(id, n)| (*id, n.row_index().unwrap()))
        .collect();
    rows.sort_by_key(|(_, index)| *index);
    rows
}
fn action(id: NodeId, action: Action) -> egui::Event {
    egui::Event::AccessKitActionRequest(ActionRequest {
        action,
        target_tree: TreeId::ROOT,
        target_node: id,
        data: None,
    })
}

#[test]
fn real_table_resize_preserves_manual_process_and_adapts_untouched_tag() {
    let mut h = table();
    assert_eq!(headers(&h)[3].bounds().unwrap().width(), 210.0);
    h.resize(900.0, 560.0);
    assert_eq!(headers(&h)[3].bounds().unwrap().width(), 140.0);
    assert_eq!(headers(&h)[4].bounds().unwrap().width(), 110.0);
    h.resize(1280.0, 820.0);
    let process = headers(&h)[3].bounds().unwrap();
    let spacing = h.ctx.style_of(egui::Theme::Dark).spacing.item_spacing.x;
    let from = egui::pos2(
        process.x1 as f32 + spacing * 0.5,
        (process.y0 + 15.0) as f32,
    );
    let to = from + egui::vec2(35.0, 0.0);
    h.frame(vec![egui::Event::PointerMoved(from)]);
    h.pointer(from, true);
    h.frame(vec![egui::Event::PointerMoved(to)]);
    h.pointer(to, false);
    h.frame(vec![]);
    let manual = headers(&h)[3].bounds().unwrap().width();
    assert!(manual > 230.0, "real divider drag took effect: {manual}");
    for width in [900.0, 1280.0, 900.0] {
        h.resize(width, 820.0);
        assert_eq!(headers(&h)[3].bounds().unwrap().width(), manual);
        assert_eq!(
            headers(&h)[4].bounds().unwrap().width(),
            if width < 1050.0 { 110.0 } else { 165.0 }
        );
    }
}

#[test]
fn visible_table_rows_expose_fields_selection_and_real_focus_activation() {
    let mut h = table();
    let table = h
        .accessible_nodes
        .iter()
        .find(|(_, n)| n.role() == Role::Table)
        .unwrap();
    assert_eq!(table.1.row_count(), Some(301));
    assert_eq!(table.1.column_count(), Some(6));
    assert!(table.1.is_multiselectable());
    let rows = rows(&h);
    assert!(rows.len() > 5 && rows.len() < 50);
    let header_ids: Vec<_> = headers(&h)
        .iter()
        .map(|header| {
            h.accessible_nodes
                .iter()
                .find(|(_, n)| std::ptr::eq(n, *header))
                .unwrap()
                .0
        })
        .collect();
    for (id, index) in &rows {
        let row = node(&h, *id);
        assert_eq!(row.is_selected(), Some(false));
        let label = row.label().unwrap();
        for field in ["Time", "Level", "PID / TID", "Process", "Tag", "Message"] {
            assert!(label.contains(field));
        }
        assert!(label.contains(&format!("message-{}", index - 1)));
        assert!(row.supports_action(Action::Click) && row.supports_action(Action::Focus));
        assert_eq!(row.children().len(), 6);
        for (column, cell) in row.children().iter().enumerate() {
            let cell = node(&h, *cell);
            assert_eq!(cell.role(), Role::Cell);
            assert_eq!(cell.row_index(), Some(*index));
            assert_eq!(cell.column_index(), Some(column));
            assert_eq!(cell.described_by(), &[header_ids[column]]);
            assert!(cell.bounds().unwrap().width() > 0.0);
        }
    }
    let (id, index) = rows[3];
    h.frame(vec![action(id, Action::Focus)]);
    h.frame(vec![]); // focus must survive a new table pass
    h.key(egui::Key::Space, egui::Modifiers::NONE);
    assert_eq!(h.app.selected, Some(index - 1));
    h.frame(vec![]);
    assert_eq!(node(&h, id).is_selected(), Some(true));
    let (id, index) = rows[5];
    let message = node(&h, node(&h, id).children()[5]).bounds().unwrap();
    h.click(egui::pos2(
        (message.x0 + 15.0) as f32,
        ((message.y0 + message.y1) * 0.5) as f32,
    ));
    assert_eq!(
        h.app.selected,
        Some(index - 1),
        "MESSAGE-area pointer activation"
    );
    h.frame(vec![action(rows[2].0, Action::Click)]);
    assert_eq!(h.app.selected, Some(rows[2].1 - 1));
    h.app.clear_logs();
    let mut entry = LogEntry::new(
        "10-11 13:00:00.123",
        1,
        1,
        "界".repeat(22000),
        Level::Info,
        "🦀".repeat(262144),
    );
    entry.process = Some(Arc::from("程".repeat(22000)));
    h.app.append_entry(entry);
    h.frame(vec![]);
    let (id, _) = self::rows(&h)[0];
    let summary = node(&h, id).label().unwrap();
    assert!(summary.len() < 8192 && summary.contains("Preview truncated"));
}

#[test]
fn arrow_navigation_starts_at_focused_row_and_keeps_focus_with_selection() {
    let mut h = table();
    let visible = rows(&h);
    h.frame(vec![action(visible[2].0, Action::Click)]);
    let (id, index) = visible[8];
    h.frame(vec![action(id, Action::Focus)]);
    assert_eq!(
        h.app.selected,
        Some(visible[2].1 - 1),
        "focus alone preserves selection"
    );
    h.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(h.app.selected, Some(index));
    h.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(h.app.selected, Some(index + 1));
    let focused = h
        .ctx
        .memory(|memory| memory.focused())
        .unwrap()
        .accesskit_id();
    assert_eq!(node(&h, focused).row_index(), Some(index + 2));
    h.key(egui::Key::ArrowUp, egui::Modifiers::SHIFT);
    assert!(h.app.selection.contains(index) && h.app.selection.contains(index + 1));
    h.app.follow_logs = true;
    h.app.scroll_to_bottom = true;
    for _ in 0..3 {
        h.frame(vec![]);
    }
    let tail = rows(&h)[0].0;
    h.frame(vec![action(tail, Action::Focus)]);
    assert!(
        !h.app.follow_logs,
        "new row focus pauses automatic tail movement"
    );
    h.key(egui::Key::End, egui::Modifiers::COMMAND);
    assert!(
        h.app.follow_logs,
        "explicit tail shortcut resumes even while row stays focused"
    );
    h.frame(vec![]);
    let tail = rows(&h)[0].0;
    for _ in 0..80 {
        h.app
            .append_entry(LogEntry::marker("same-frame incoming capture"));
    }
    h.frame(vec![action(tail, Action::Focus)]);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert!(!h.app.follow_logs);
    assert_eq!(
        h.ctx.memory(|m| m.focused()).map(|id| id.accesskit_id()),
        Some(tail)
    );
    assert!(h.accessible_nodes.iter().any(|(id, _)| *id == tail));
    h.key(egui::Key::End, egui::Modifiers::COMMAND);
    assert!(h.app.follow_logs);
}

#[test]
fn unselected_keyboard_row_has_clipped_visible_focus_in_both_themes() {
    for dark in [true, false] {
        let mut h = table();
        h.app.dark = dark;
        configure_style(&h.ctx, dark);
        h.resize(900.0, 560.0);
        let (id, _) = rows(&h)[3];
        h.frame(vec![action(id, Action::Focus)]);
        h.frame(vec![]);
        assert!(h.app.selection.is_empty());
        let bounds = node(&h, id).bounds().unwrap();
        let row = egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        );
        let expected = h
            .ctx
            .style_of(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            })
            .visuals
            .text_color();
        fn rings(shape: &egui::Shape, target: egui::Rect, color: Color32) -> bool {
            match shape {
                egui::Shape::Rect(r) => {
                    r.rect == target
                        && r.stroke.width == 1.0
                        && r.stroke.color == color
                        && r.fill == Color32::TRANSPARENT
                }
                egui::Shape::Vec(shapes) => shapes.iter().any(|s| rings(s, target, color)),
                _ => false,
            }
        }
        let painted = h
            .shapes
            .iter()
            .find(|s| rings(&s.shape, row.shrink(0.5), expected))
            .expect("actual painted focused-row outline");
        assert!(painted.clip_rect.min.x >= 0.0 && painted.clip_rect.max.x <= 900.0);
        assert!(painted.clip_rect.min.y >= row.min.y && painted.clip_rect.max.y <= row.max.y);
        h.click(egui::pos2(row.max.x - 30.0, row.center().y));
        assert!(
            !h.app.selection.is_empty(),
            "focus outline must not cover row pointer input"
        );
    }
}

#[test]
fn global_navigation_focuses_visible_and_virtualized_targets() {
    let mut h = table();
    assert!(h.ctx.memory(|m| m.focused()).is_none());
    h.key(egui::Key::Home, egui::Modifiers::NONE);
    h.frame(vec![]);
    let focused = h
        .ctx
        .memory(|m| m.focused())
        .expect("Home gives row focus")
        .accesskit_id();
    assert_eq!(node(&h, focused).row_index(), Some(1));
    h.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(h.app.selected, Some(1));
    h.key(egui::Key::End, egui::Modifiers::NONE);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    let focused = h
        .ctx
        .memory(|m| m.focused())
        .expect("offscreen End target gets row focus")
        .accesskit_id();
    assert_eq!(node(&h, focused).row_index(), Some(300));
    assert_eq!(h.app.selected, Some(299));
    h.key(egui::Key::ArrowUp, egui::Modifiers::NONE);
    assert_eq!(h.app.selected, Some(298));
}

#[test]
fn row_identity_and_focus_survive_details_open_close() {
    let mut h = table();
    h.app.show_details = true;
    h.frame(vec![]);
    let before = rows(&h);
    let (id, index) = before[3];
    h.frame(vec![action(id, Action::Click)]);
    h.frame(vec![]);
    let after = rows(&h);
    assert_eq!(after.iter().find(|(_, row)| *row == index).unwrap().0, id);
    assert_eq!(
        h.ctx.memory(|m| m.focused()).map(|id| id.accesskit_id()),
        Some(id)
    );
    h.app.show_details = false;
    h.frame(vec![]);
    assert_eq!(
        rows(&h).iter().find(|(_, row)| *row == index).unwrap().0,
        id
    );
    assert_eq!(
        h.ctx.memory(|m| m.focused()).map(|id| id.accesskit_id()),
        Some(id)
    );
    let before_scroll = rows(&h);
    let old_cells = node(&h, id).children().to_vec();
    h.frame(vec![
        egui::Event::PointerMoved(egui::pos2(600.0, 300.0)),
        egui::Event::MouseWheel {
            phase: egui::TouchPhase::Move,
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -64.0),
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    h.frame(vec![]);
    let after_scroll = rows(&h);
    for (old, index) in before_scroll {
        if let Some((new, _)) = after_scroll.iter().find(|(_, row)| *row == index) {
            assert_eq!(old, *new);
        }
    }
    assert_eq!(node(&h, id).children(), old_cells.as_slice());
    h.app.clear_logs();
    h.app.append_entry(LogEntry::new(
        "10-11 13:00:00.123",
        1,
        1,
        "new",
        Level::Info,
        "new log after clear",
    ));
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_ne!(self::rows(&h)[0].0, id);
}
