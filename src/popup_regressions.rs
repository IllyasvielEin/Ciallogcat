//! Real egui frame sequences: press and release must never be collapsed into one frame.
use super::*;

use super::ui_test_support::Harness;

#[test]
fn first_and_repeated_pointer_clicks_keep_both_input_popups_open() {
    for populated in [false, true] {
        for id in ["query_shortcut", "package_filter"] {
            let mut h = Harness::new(populated);
            for cycle in 0..2 {
                let pos = h.input_pos(id);
                h.pointer(pos, true);
                assert!(
                    h.open(id),
                    "{id}, populated={populated}, cycle={cycle}: press"
                );
                h.pointer(pos, false);
                assert!(
                    h.open(id),
                    "{id}, populated={populated}, cycle={cycle}: release"
                );
                h.frame(vec![]);
                assert!(h.open(id), "{id}: settled");
                assert_eq!(h.ctx.memory(|m| m.focused()), Some(egui::Id::new(id)));
                h.click_input(id);
                assert!(h.open(id), "{id}: second click");
                h.click(egui::pos2(40.0, 500.0));
                assert!(!h.open(id), "{id}: outside click");
            }
        }
    }
}

#[test]
fn keyboard_focus_escape_and_enter_preserve_input_popup_contract() {
    for populated in [false, true] {
        let mut h = Harness::new(populated);
        h.key(egui::Key::F, egui::Modifiers::COMMAND);
        assert!(h.app.query_history_open);
        h.frame(vec![egui::Event::Text("new query".to_owned())]);
        assert!(h.app.query_history_open);
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert!(!h.app.query_history_open);
        assert_eq!(h.app.filter_history[0], "new query");
        h.click_input("query_shortcut");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.query_history_open);
        assert!(!h.app.package_popup_open);
        assert!(!h.ctx.text_edit_focused());
        h.key(egui::Key::Slash, egui::Modifiers::NONE);
        assert!(h.app.query_history_open);
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        h.click_input("package_filter");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.package_popup_open);
    }
    let mut h = Harness::new(true);
    let mut reached_package = false;
    let mut reached_query = false;
    for _ in 0..20 {
        h.key(egui::Key::Tab, egui::Modifiers::NONE);
        let focus = h.ctx.memory(|m| m.focused());
        if focus == Some(egui::Id::new("package_filter")) {
            assert!(h.app.package_popup_open);
            reached_package = true;
        }
        if focus == Some(egui::Id::new("query_shortcut")) {
            assert!(h.app.query_history_open);
            assert!(
                !h.app.package_popup_open,
                "Tab transfers the active input popup"
            );
            reached_query = true;
            break;
        }
    }
    assert!(
        reached_package && reached_query,
        "Tab must reach and open both inputs"
    );
}

#[test]
fn popup_selection_applies_value_and_closes_without_reopening() {
    let mut h = Harness::new(true);
    h.click_input("query_shortcut");
    h.click_label("timeout");
    assert_eq!(h.app.query, "timeout");
    assert!(!h.app.query_history_open);
    h.frame(vec![]);
    assert!(!h.app.query_history_open);
    h.click_input("package_filter");
    h.click_label("com.example.app");
    assert_eq!(h.app.package, "com.example.app");
    assert!(!h.app.package_popup_open);
    h.frame(vec![]);
    assert!(!h.app.package_popup_open);
    h.click_input("package_filter");
    h.click_label("全部进程");
    assert!(h.app.package.is_empty());
    assert!(!h.app.package_popup_open);
}

#[test]
fn switching_input_anchors_leaves_only_the_clicked_popup_open() {
    let mut h = Harness::new(true);
    for _ in 0..3 {
        h.click_input("package_filter");
        assert!(h.app.package_popup_open);
        assert!(!h.app.query_history_open);
        h.click_input("query_shortcut");
        assert!(h.app.query_history_open);
        assert!(!h.app.package_popup_open);
    }
}

#[test]
fn escape_from_popup_child_closes_popup_before_clearing_log_selection() {
    for id in ["query_shortcut", "package_filter"] {
        let mut h = Harness::new(true);
        h.app.append_entry(LogEntry::new(
            "",
            1,
            1,
            "Test",
            Level::Info,
            "retained selection",
        ));
        h.app.selected = Some(0);
        h.app.selection.select(&[0], 0, false, false);
        h.click_input(id);
        h.key(egui::Key::Tab, egui::Modifiers::NONE);
        assert!(!h.ctx.text_edit_focused(), "Tab must reach a popup child");
        assert!(h.open(id));
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.open(id), "Escape must dismiss {id} with child focus");
        assert_eq!(
            h.app.selected,
            Some(0),
            "popup dismissal must preserve row selection"
        );
        assert!(!h.app.selection.is_empty());
    }
}

#[test]
fn escape_closes_view_menu_then_clears_selection_on_the_next_press() {
    let mut h = Harness::new(true);
    h.app.append_entry(LogEntry::marker("retained selection"));
    h.app.selected = Some(0);
    h.app.selection.select_all(&h.app.matches);
    h.frame(vec![]);
    h.click_label("视图");
    assert!(egui::Popup::is_any_open(&h.ctx));
    h.key(egui::Key::Escape, egui::Modifiers::NONE);
    assert!(!egui::Popup::is_any_open(&h.ctx));
    assert_eq!(h.app.selected, Some(0));
    assert!(!h.app.selection.is_empty());
    h.key(egui::Key::Escape, egui::Modifiers::NONE);
    assert_eq!(h.app.selected, None);
    assert!(h.app.selection.is_empty());
}

#[test]
fn popup_child_keyboard_input_never_operates_on_underlying_log_rows() {
    for id in ["query_shortcut", "package_filter"] {
        let mut h = Harness::new(true);
        h.app.append_entry(LogEntry::marker("first"));
        h.app.append_entry(LogEntry::marker("second"));
        h.app.selected = Some(0);
        h.app.selection.select(&h.app.matches, 0, false, false);
        h.click_input(id);
        h.key(egui::Key::Tab, egui::Modifiers::NONE);
        assert!(!h.ctx.text_edit_focused());
        h.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
        assert_eq!(h.app.selected, Some(0), "{id}: arrows belong to popup");
        h.key(egui::Key::A, egui::Modifiers::COMMAND);
        assert!(
            !h.app.selection.contains(1),
            "{id}: Select All belongs to popup"
        );
        h.frame(vec![egui::Event::Copy]);
        assert!(
            h.copied.is_empty(),
            "{id}: must not copy background log rows"
        );
    }
}

#[test]
fn history_delete_clear_and_keyboard_selection_remain_local_to_popup() {
    let mut h = Harness::new(true);
    h.click_input("query_shortcut");
    h.click_label("×");
    assert_eq!(h.app.filter_history, ["fatal"]);
    assert!(
        h.app.query_history_open,
        "deleting one item keeps the list open"
    );
    h.click_label("清空历史");
    assert!(h.app.filter_history.is_empty());
    assert!(!h.app.query_history_open);
    h.click_input("query_shortcut");
    assert!(h.labels.iter().any(|(label, _)| label == "暂无筛选历史"));

    let mut h = Harness::new(true);
    h.key(egui::Key::F, egui::Modifiers::COMMAND);
    h.key(egui::Key::Tab, egui::Modifiers::NONE); // Popup area.
    h.key(egui::Key::Tab, egui::Modifiers::NONE); // First history item.
    assert!(!h.ctx.text_edit_focused());
    h.key(egui::Key::Enter, egui::Modifiers::NONE);
    assert_eq!(h.app.query, "timeout");
    assert!(!h.app.query_history_open);
}

#[test]
fn find_shortcut_replaces_existing_menu_and_package_popup() {
    let mut h = Harness::new(true);
    h.click_label("视图");
    assert!(egui::Popup::is_any_open(&h.ctx));
    h.key(egui::Key::F, egui::Modifiers::COMMAND);
    assert!(!egui::Popup::is_any_open(&h.ctx));
    assert!(h.app.query_history_open);
    assert_eq!(
        h.ctx.memory(|m| m.focused()),
        Some(egui::Id::new("query_shortcut"))
    );
    h.click_input("package_filter");
    assert!(h.app.package_popup_open);
    h.key(egui::Key::F, egui::Modifiers::COMMAND);
    assert!(!h.app.package_popup_open);
    assert!(h.app.query_history_open);
}

#[test]
fn selecting_history_after_typing_does_not_reorder_rows_under_pointer() {
    let mut h = Harness::new(true);
    h.click_input("query_shortcut");
    h.frame(vec![egui::Event::Text("current query".to_owned())]);
    h.frame(vec![]);
    assert_eq!(h.app.filter_history, ["timeout", "fatal"]);
    h.click_label("fatal");
    assert_eq!(h.app.query, "fatal", "release must choose the pressed row");
    assert_eq!(h.app.filter_history, ["fatal", "current query", "timeout"]);
    assert!(!h.app.query_history_open);
}

#[test]
fn deleting_and_clearing_history_with_pending_text_keep_row_identity_and_intent() {
    let mut h = Harness::new(true);
    h.click_input("query_shortcut");
    h.frame(vec![egui::Event::Text("current query".to_owned())]);
    h.frame(vec![]);
    h.click_label("×");
    assert_eq!(
        h.app.filter_history,
        ["fatal"],
        "delete the row actually clicked"
    );
    assert!(h.app.query_history_open);
    h.click(egui::pos2(40.0, 500.0));
    assert_eq!(
        h.app.filter_history,
        ["current query", "fatal"],
        "record pending text after dismissal"
    );

    let mut h = Harness::new(true);
    h.click_input("query_shortcut");
    h.frame(vec![egui::Event::Text("current query".to_owned())]);
    h.frame(vec![]);
    h.click_label("清空历史");
    assert!(
        h.app.filter_history.is_empty(),
        "Clear must not immediately reinsert pending text"
    );
    assert!(!h.app.query_history_pending);
    assert_eq!(h.app.query, "current query");
    h.app.record_query_history();
    assert!(h.app.filter_history.is_empty());
}

#[test]
fn resized_details_height_persists_for_short_long_and_segmented_logs() {
    let mut h = Harness::new(false);
    for message in [
        "short".to_owned(),
        "line\n".repeat(80),
        "segment\n".repeat(3000),
    ] {
        h.app
            .append_entry(LogEntry::new("", 1, 1, "Test", Level::Info, message));
    }
    h.app.selected = Some(0);
    h.app.selection.select(&h.app.matches, 0, false, false);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    let panel = |ctx: &egui::Context| {
        egui::containers::panel::PanelState::load(ctx, egui::Id::new("details"))
            .unwrap()
            .outer_rect
    };
    let before = panel(&h.ctx);
    let start = egui::pos2(before.center().x, before.top());
    let end = start - egui::vec2(0.0, 60.0);
    h.pointer(start, true);
    for step in 1..=6 {
        h.frame(vec![egui::Event::PointerMoved(
            start + (end - start) * (step as f32 / 6.0),
        )]);
    }
    h.pointer(end, false);
    for _ in 0..20 {
        h.frame(vec![]);
    }
    let resized = panel(&h.ctx);
    assert!(
        resized.height() > before.height() + 25.0,
        "resize must stick: before={before:?}, after={resized:?}"
    );
    assert_eq!(h.app.selected, Some(0));
    assert!(h.app.selection.contains(0));
    for index in [1, 2, 0] {
        h.app.selected = Some(index);
        h.app.selection.select(&h.app.matches, index, false, false);
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(
            (panel(&h.ctx).height() - resized.height()).abs() < 1.0,
            "selected row {index} must preserve chosen height"
        );
    }
}
