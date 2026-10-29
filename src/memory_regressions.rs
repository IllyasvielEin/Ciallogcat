//! Deterministic state transitions and real egui-input memory regressions.
use super::ui_test_support::Harness;
use super::*;

fn app(rows: usize) -> (egui::Context, CiallogcatApp) {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.follow_logs = false;
    for index in 0..rows {
        app.append_entry(LogEntry::marker(format!(
            "row-{index} {}",
            if index % 2 == 0 { "keep" } else { "skip" }
        )));
    }
    (ctx, app)
}

fn start_revision(app: &mut CiallogcatApp) {
    app.filter_tx.cancel();
    app.filter_generation = app.filter_generation.wrapping_add(1);
    app.active_filter = app.current_filter();
    app.incremental_matcher = filter::compile_matcher(&app.active_filter);
    app.filter_pending = true;
    app.filter_progress = None;
}

fn result(app: &CiallogcatApp, processed_len: usize) -> filter::FilterResult {
    let matcher = filter::compile_matcher(&app.active_filter).unwrap();
    filter::FilterResult {
        generation: app.filter_generation,
        log_epoch: app.log_epoch,
        snapshot_base: app.dropped_entries,
        processed_len,
        matches: (0..processed_len)
            .filter(|&i| {
                filter::entry_matches(&app.entries[i], &app.active_filter, matcher.as_ref())
            })
            .collect(),
        elapsed: Duration::ZERO,
        error: None,
        matcher,
    }
}

fn copy(app: &mut CiallogcatApp, ctx: &egui::Context, target: CopyTarget) -> Vec<String> {
    let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| app.copy_logs(ctx, target));
    output.textures_delta.clear();
    output
        .platform_output
        .commands
        .into_iter()
        .filter_map(|command| {
            if let egui::OutputCommand::CopyText(text) = command {
                Some(text)
            } else {
                None
            }
        })
        .collect()
}

fn slice(app: &mut CiallogcatApp, rows: usize) {
    app.advance_filter_catchup(rows, usize::MAX, Duration::MAX);
}

#[test]
fn catchup_publishes_prefix_and_advances_exactly_across_many_turns() {
    let (_, mut app) = app(8201);
    app.query = "keep".to_owned();
    start_revision(&mut app);
    app.accept_filter_result(result(&app, 1));
    assert_eq!(app.matches, [0]);
    assert!(app.filter_pending);
    for expected_end in [2049, 4097, 6145, 8193, 8201] {
        slice(&mut app, FILTER_CATCHUP_ROWS);
        assert_eq!(app.filter_progress.unwrap().next_absolute, expected_end);
        assert_eq!(
            app.matches,
            (0..expected_end).step_by(2).collect::<Vec<_>>()
        );
    }
    assert!(!app.filter_pending);
    app.append_entry(LogEntry::marker("keep tail"));
    assert_eq!(app.matches.last(), Some(&8201));
    assert_eq!(app.filter_progress.unwrap().next_absolute, 8202);
}

#[test]
fn continuous_ingress_never_withholds_partial_results() {
    let (_, mut app) = app(4096);
    start_revision(&mut app);
    app.accept_filter_result(result(&app, 0));
    for turn in 1..=8 {
        for _ in 0..1500 {
            app.append_entry(LogEntry::marker("arriving"));
        }
        slice(&mut app, 1024);
        assert_eq!(app.matches.len(), turn * 1024);
        assert_eq!(app.filter_progress.unwrap().next_absolute, turn * 1024);
        assert!(app.filter_pending);
    }
    while app.filter_pending {
        slice(&mut app, 2048);
    }
    assert_eq!(app.matches, (0..app.entries.len()).collect::<Vec<_>>());
}

#[test]
fn pending_selection_is_retained_pruned_only_when_known_and_never_autocopied() {
    let (ctx, mut app) = app(12);
    app.selection.select(&app.matches, 2, false, false);
    app.selection.select(&app.matches, 10, false, true);
    app.selection.select(&app.matches, 9, false, true);
    app.selected = Some(10);
    app.detail_lines = Some((
        Arc::clone(&app.entries[10].message),
        std::iter::once(0..1).collect(),
    ));
    app.query = "keep".to_owned();
    start_revision(&mut app);
    assert!(copy(&mut app, &ctx, CopyTarget::Selection).is_empty());
    app.accept_filter_result(result(&app, 4));
    assert_eq!(app.selection.indices().collect::<Vec<_>>(), [2, 9, 10]);
    assert_eq!(app.selected, Some(10));
    assert!(app.detail_lines.is_some());
    assert!(copy(&mut app, &ctx, CopyTarget::Selection).is_empty());
    slice(&mut app, 6);
    assert_eq!(app.selection.indices().collect::<Vec<_>>(), [2, 10]);
    assert!(copy(&mut app, &ctx, CopyTarget::Selection).is_empty());
    slice(&mut app, 2);
    // No delayed side effect: only another explicit request copies.
    let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| app.poll_filter());
    output.textures_delta.clear();
    assert!(
        !output
            .platform_output
            .commands
            .iter()
            .any(|c| matches!(c, egui::OutputCommand::CopyText(_)))
    );
    let expected = format!(
        "{}\n{}",
        app.entries[2].copy_text(),
        app.entries[10].copy_text()
    );
    assert_eq!(copy(&mut app, &ctx, CopyTarget::Selection), [expected]);
}

#[test]
fn revision_change_invalidates_old_watermark_and_clear_invalidates_old_result() {
    let (ctx, mut app) = app(8);
    app.selection.select_all(&app.matches);
    let stale = result(&app, 8);
    start_revision(&mut app);
    app.accept_filter_result(stale);
    assert!(app.filter_progress.is_none());
    assert!(copy(&mut app, &ctx, CopyTarget::Selection).is_empty());
    let stale = result(&app, 8);
    app.clear_logs();
    app.append_entry(LogEntry::marker("new epoch"));
    app.accept_filter_result(stale);
    assert!(app.matches.is_empty());
    assert_eq!(app.entries.len(), 1);
}

#[test]
fn snapshot_acceptance_and_selection_rebase_over_repeated_prefix_eviction() {
    let (_, mut app) = app(12);
    app.query = "keep".to_owned();
    start_revision(&mut app);
    let snapshot = result(&app, 8);
    app.selection.select(&app.matches, 2, false, false);
    app.selection.select(&app.matches, 10, false, true);
    app.selected = Some(10);
    let id = egui::Id::new(("log_row", app.log_epoch, 10));
    app.focused_log_row = Some((id, 10));
    app.visible_log_rows = vec![(id, 10)];
    app.detail_lines = Some((
        Arc::clone(&app.entries[10].message),
        std::iter::once(0..1).collect(),
    ));
    let generation = app.filter_generation;
    Arc::make_mut(&mut app.entries).drain_front(3);
    app.rebase_after_trim(3);
    Arc::make_mut(&mut app.entries).drain_front(2);
    app.rebase_after_trim(2);
    app.accept_filter_result(snapshot);
    assert_eq!(app.matches, [1]);
    assert_eq!(app.selection.indices().collect::<Vec<_>>(), [5]);
    assert_eq!(app.selected, Some(5));
    assert_eq!(app.focused_log_row, Some((id, 5)));
    assert_eq!(app.visible_log_rows, [(id, 10)]);
    assert!(app.detail_lines.is_some());
    assert_eq!(app.filter_generation, generation);
    slice(&mut app, 32);
    assert_eq!(app.matches, [1, 3, 5]);
    assert_eq!(app.entries[5].message.as_ref(), "row-10 keep");
}

#[test]
fn process_sample_does_not_rewrite_history_or_commit_filter_draft() {
    let (_, mut app) = app(0);
    app.package = "com.old".to_owned();
    app.schedule_filter();
    app.append_entry(LogEntry::new("", 3, 3, "Tag", Level::Info, "old"));
    app.query = "draft".to_owned();
    app.queue_filter();
    let generation = app.filter_generation;
    app.update_processes(HashMap::from([(3, "com.old".to_owned())]));
    assert_eq!(app.active_filter.query, "");
    assert!(app.filter_dirty_since.is_some());
    assert!(app.entries[0].process.is_none());
    assert_eq!(app.filter_generation, generation);
}

#[test]
fn applying_budget_trims_once_and_keeps_only_applied_config() {
    let (_, mut app) = app(0);
    for _ in 0..100 {
        app.append_entry(LogEntry::marker("x".repeat(256 * 1024)));
    }
    let before = app.entries.len();
    app.memory_limit_draft = Some(16);
    assert_eq!(app.app_config().memory_limit_mib, 300);
    assert_eq!(app.entries.len(), before);
    app.apply_memory_budget(16);
    assert_eq!(app.app_config().memory_limit_mib, 16);
    assert!(app.entries.memory_bytes() <= memory_budget_bytes(16) * 9 / 10);
    let retained = app.entries.len();
    let dropped = app.dropped_entries;
    app.apply_memory_budget(16);
    app.apply_memory_budget(32);
    assert_eq!(app.entries.len(), retained);
    assert_eq!(app.dropped_entries, dropped);
    assert_eq!(
        memory_budget_bytes(4096),
        (4096u64 * MIB as u64).min(usize::MAX as u64) as usize
    );
}

#[test]
fn budget_cancel_escape_outside_and_reopen_discard_draft_using_real_input() {
    let (ctx, app) = app(10);
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    h.click_rendered_label("View");
    h.app.memory_limit_draft = Some(16);
    h.click_rendered_label("Cancel budget");
    assert_eq!(h.app.memory_limit_mib, 300);
    assert_eq!(h.app.entries.len(), 10);
    h.click_rendered_label("View");
    assert_eq!(h.app.memory_limit_draft, Some(300));
    h.app.memory_limit_draft = Some(32);
    h.key(egui::Key::Escape, egui::Modifiers::NONE);
    assert!(h.app.memory_limit_draft.is_none());
    h.click_rendered_label("View");
    h.app.memory_limit_draft = Some(64);
    h.click(egui::pos2(800.0, 500.0));
    assert!(h.app.memory_limit_draft.is_none());
    h.click_rendered_label("View");
    assert_eq!(h.app.memory_limit_draft, Some(300));
    h.app.memory_limit_draft = Some(64);
    h.click_rendered_label("Apply budget");
    assert_eq!(h.app.memory_limit_mib, 64);
    assert_eq!(h.app.app_config().memory_limit_mib, 64);
}

#[test]
fn sparse_fractional_viewport_offset_is_restored_after_repeated_trim() {
    let (ctx, mut app) = app(300);
    app.show_details = false;
    app.matches = (0..300).step_by(3).collect();
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    h.app.restore_scroll_offset = Some(321.5);
    h.frame(vec![]);
    let old = h.app.table_scroll_offset;
    let stride = h.app.table_row_stride;
    Arc::make_mut(&mut h.app.entries).drain_front(4);
    h.app.rebase_after_trim(4); // two matching rows removed
    Arc::make_mut(&mut h.app.entries).drain_front(2);
    h.app.rebase_after_trim(2); // no additional matching row removed
    h.frame(vec![]);
    assert!((h.app.table_scroll_offset - (old - 2.0 * stride)).abs() < 0.1);
    assert!(!h.app.follow_logs);
}

fn click_copy_label(h: &mut Harness, label: &str) {
    let pos = h
        .labels
        .iter()
        .find(|(text, _)| text == label)
        .unwrap_or_else(|| panic!("missing {label}: {:?}", h.labels))
        .1;
    h.pointer(pos, true);
    h.pointer(pos, false);
}

fn open_row_menu(h: &mut Harness) {
    let pos = h
        .labels
        .iter()
        .find(|(text, pos)| text.starts_with("[event] row-0 keep") && pos.x > 500.0)
        .expect("visible row")
        .1;
    for pressed in [true, false] {
        h.frame(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
    h.frame(vec![]);
}

#[test]
fn all_four_copy_entrypoints_block_pending_membership_and_copy_only_on_retry() {
    for route in 0..4 {
        let (ctx, mut app) = app(2);
        app.selected = Some(0);
        app.selection.select_all(&app.matches);
        let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
        start_revision(&mut h.app);
        h.frame(vec![]);
        assert!(
            h.labels
                .iter()
                .any(|(label, _)| label == "Selected log · checking filter…")
        );
        match route {
            0 => h.frame(vec![egui::Event::Copy]),
            1 => click_copy_label(&mut h, "Copy"),
            2 | 3 => {
                open_row_menu(&mut h);
                click_copy_label(
                    &mut h,
                    if route == 2 {
                        "Copy selected logs"
                    } else {
                        "Copy full log"
                    },
                );
            }
            _ => unreachable!(),
        }
        assert!(h.copied.is_empty());
        assert!(
            h.app
                .copy_notice
                .as_ref()
                .unwrap()
                .contains("still being checked")
        );
        let ready = result(&h.app, 2);
        h.app.accept_filter_result(ready);
        h.frame(vec![]);
        assert!(h.copied.is_empty());
        // Close any menu and retry explicitly through the common route.
        egui::Popup::close_all(&h.ctx);
        h.frame(vec![egui::Event::Copy]);
        assert_eq!(h.copied.len(), 1);
        assert!(h.copied[0].contains("row-0 keep"));
        assert!(h.copied[0].contains("row-1 skip"));
    }
}

#[test]
fn oversized_single_and_multirow_copy_never_emit_partial_clipboard_commands() {
    let (ctx, mut app) = app(1);
    app.append_entry(LogEntry::marker("x".repeat(copy::MAX_COPY_BYTES)));
    app.selection.select_all(&app.matches);
    assert!(copy(&mut app, &ctx, CopyTarget::Row(1)).is_empty());
    assert!(
        app.copy_notice
            .as_ref()
            .unwrap()
            .contains("clipboard limit")
    );
    assert!(copy(&mut app, &ctx, CopyTarget::Selection).is_empty());
    assert!(
        app.copy_notice
            .as_ref()
            .unwrap()
            .contains("clipboard limit")
    );
    assert_eq!(copy(&mut app, &ctx, CopyTarget::Row(0)).len(), 1);
}

#[test]
fn surviving_prior_frame_accessible_target_is_rebased_before_tail_layout() {
    use egui::accesskit::{Action, ActionRequest, TreeId};
    let (ctx, mut app) = app(300);
    ctx.enable_accesskit();
    app.show_details = false;
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    h.app.restore_scroll_offset = Some(600.5);
    h.frame(vec![]);
    let (id, absolute) = h.app.visible_log_rows[4];
    let removed = absolute - 1;
    h.app.follow_logs = true; // action and trim can arrive during a live tail
    Arc::make_mut(&mut h.app.entries).drain_front(removed);
    h.app.rebase_after_trim(removed);
    h.app.follow_logs = true;
    h.app.scroll_to_bottom = true;
    h.frame(vec![egui::Event::AccessKitActionRequest(ActionRequest {
        action: Action::Click,
        target_tree: TreeId::ROOT,
        target_node: id.accesskit_id(),
        data: None,
    })]);
    assert!(!h.app.follow_logs);
    assert_eq!(h.app.selected, Some(1));
    assert_eq!(
        h.app.entries[1].message.as_ref(),
        format!(
            "row-{absolute} {}",
            if absolute % 2 == 0 { "keep" } else { "skip" }
        )
    );
}

#[test]
fn held_pointer_target_stays_at_same_screen_position_through_prefix_trim() {
    let (ctx, mut app) = app(300);
    app.show_details = false;
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    h.app.restore_scroll_offset = Some(600.5);
    h.frame(vec![]);
    let (label, pos) = h
        .labels
        .iter()
        .find(|(label, _)| label.starts_with("[event] row-25 "))
        .expect("target row")
        .clone();
    h.pointer(pos, true);
    Arc::make_mut(&mut h.app.entries).drain_front(10);
    h.app.rebase_after_trim(10);
    h.frame(vec![egui::Event::PointerMoved(pos)]);
    h.pointer(pos, false);
    assert_eq!(h.app.selected, Some(15));
    assert_eq!(
        h.app.entries[15].message.as_ref(),
        label.strip_prefix("[event] ").unwrap()
    );
}

#[test]
fn budget_value_real_typing_and_dragging_do_not_apply_or_close_the_menu() {
    let (ctx, app) = app(10);
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    h.click_rendered_label("View");
    h.click_rendered_label("300");
    assert!(
        h.app.memory_limit_draft.is_some(),
        "click must enter editing, not close View"
    );
    h.frame(vec![egui::Event::Text("16".to_owned())]);
    assert_eq!(h.app.memory_limit_draft, Some(16));
    assert_eq!(h.app.memory_limit_mib, 300);
    assert_eq!(h.app.app_config().memory_limit_mib, 300);
    h.click_rendered_label("Cancel budget");
    h.click_rendered_label("View");
    assert_eq!(h.app.memory_limit_draft, Some(300));
    let pos = h
        .labels
        .iter()
        .find(|(label, _)| label == "300")
        .expect("budget value")
        .1;
    h.pointer(pos, true);
    h.frame(vec![egui::Event::PointerMoved(pos - egui::vec2(30.0, 0.0))]);
    h.pointer(pos - egui::vec2(30.0, 0.0), false);
    let dragged = h.app.memory_limit_draft.expect("drag must keep View open");
    assert!(dragged < 300);
    assert_eq!(h.app.memory_limit_mib, 300);
    h.click_rendered_label("Cancel budget");
    h.click_rendered_label("View");
    h.click_rendered_label("300");
    h.frame(vec![egui::Event::Text("32".to_owned())]);
    h.key(egui::Key::Enter, egui::Modifiers::NONE);
    assert_eq!(h.app.memory_limit_draft, Some(32));
    assert_eq!(h.app.memory_limit_mib, 300);
    h.click_rendered_label("Apply budget");
    assert_eq!(h.app.memory_limit_mib, 32);
    assert_eq!(h.app.app_config().memory_limit_mib, 32);
}
