//! Headless state and real egui-input regression tests for the quality review.
use super::*;

fn app() -> (egui::Context, CiallogcatApp) {
    let ctx = egui::Context::default();
    let app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    (ctx, app)
}
fn row() -> LogEntry {
    LogEntry::new(
        "",
        9999,
        9999,
        "AndroidRuntime",
        Level::Fatal,
        "FATAL EXCEPTION",
    )
}
fn settle(app: &mut CiallogcatApp) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.filter_pending && Instant::now() < deadline {
        app.poll_filter();
        std::thread::yield_now();
    }
    assert!(!app.filter_pending);
}
fn frame(app: &mut CiallogcatApp, ctx: &egui::Context, key: Option<(egui::Key, egui::Modifiers)>) {
    let events = key
        .map(|(key, modifiers)| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        })
        .into_iter()
        .collect();
    let mut output = ctx.run_ui(
        egui::RawInput {
            events,
            ..Default::default()
        },
        |_ui| {
            app.shortcuts(ctx);
            app.dialogs(ctx);
        },
    );
    output.textures_delta.clear();
}
#[test]
fn reselect_paused_device_preserves_history() {
    let (ctx, mut app) = app();
    app.switch_device("fixture".to_owned(), &ctx);
    app.append_entry(row());
    app.pause_capture();
    app.switch_device("fixture".to_owned(), &ctx);
    assert_eq!(app.entries.len(), 1);
    assert!(matches!(app.capture_state, CaptureState::Paused));
}
#[test]
fn reselect_disconnected_device_preserves_history() {
    let (ctx, mut app) = app();
    app.switch_device("fixture".to_owned(), &ctx);
    app.append_entry(row());
    app.reconcile_device_selection(&ctx);
    assert!(app.selected_device.is_none());
    app.switch_device("fixture".to_owned(), &ctx);
    assert_eq!(app.entries.len(), 1);
}
#[test]
fn invalid_regex_clears_stale_results_and_recovers() {
    let (_, mut app) = app();
    app.append_entry(row());
    assert_eq!(app.matches, [0]);
    app.selected = Some(0);
    app.selection.select_all(&app.matches);
    app.schedule_filter(); // a stale valid generation may still be in flight
    app.query = "[".to_owned();
    app.use_regex = true;
    app.schedule_filter();
    settle(&mut app); // Invalid expressions are now compiled off the UI thread.
    assert!(app.matches.is_empty());
    assert!(app.selected.is_none());
    assert!(app.selection.is_empty());
    assert!(app.detail_lines.is_none());
    settle(&mut app);
    assert!(app.filter_error.is_some());
    assert!(app.matches.is_empty());
    app.query = "FATAL".to_owned();
    app.schedule_filter();
    settle(&mut app);
    assert_eq!(app.matches, [0]);
}
#[test]
fn save_dialog_escape_cancels() {
    let (ctx, mut app) = app();
    app.show_save_filter = true;
    frame(&mut app, &ctx, None);
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Escape, egui::Modifiers::NONE)),
    );
    assert!(!app.show_save_filter);
    assert!(app.saved_filters.is_empty());
}

#[test]
fn save_dialog_tab_shift_tab_and_enter_work() {
    let (ctx, mut app) = app();
    app.show_save_filter = true;
    app.save_filter_name = "Crash".to_owned();
    frame(&mut app, &ctx, None);
    frame(&mut app, &ctx, None);
    let name = egui::Id::new("save_filter_name");
    assert_eq!(ctx.memory(|m| m.focused()), Some(name));
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Tab, egui::Modifiers::NONE)),
    );
    frame(&mut app, &ctx, None);
    assert_ne!(ctx.memory(|m| m.focused()), Some(name));
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Tab, egui::Modifiers::SHIFT)),
    );
    frame(&mut app, &ctx, None);
    assert_eq!(ctx.memory(|m| m.focused()), Some(name));
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Enter, egui::Modifiers::NONE)),
    );
    assert!(!app.show_save_filter);
    assert_eq!(app.saved_filters.len(), 1);
    assert_eq!(app.saved_filters[0].name, "Crash");
    app.show_save_filter = true;
    frame(&mut app, &ctx, None);
    assert_eq!(ctx.memory(|m| m.focused()), Some(name));
}

#[test]
fn device_change_and_clear_require_confirmation_escape_preserves_rows() {
    let (ctx, mut app) = app();
    app.switch_device("first".to_owned(), &ctx);
    app.append_entry(row());
    app.request_device_switch("second".to_owned(), &ctx);
    assert_eq!(app.selected_device.as_deref(), Some("first"));
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.pending_device.as_deref(), Some("second"));
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Escape, egui::Modifiers::NONE)),
    );
    assert!(app.pending_device.is_none());
    app.pending_clear = true;
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Escape, egui::Modifiers::NONE)),
    );
    assert!(!app.pending_clear);
    assert_eq!(app.entries.len(), 1);
    app.switch_device("second".to_owned(), &ctx);
    assert!(app.entries.is_empty());
}

#[test]
fn unresolved_crash_is_counted_without_guessing_package_and_survives_close() {
    let (ctx, mut app) = app();
    app.switch_device("fixture".to_owned(), &ctx);
    app.append_entry(row());
    app.package = "com.example.alpha".to_owned();
    app.schedule_filter();
    settle(&mut app);
    assert!(app.matches.is_empty());
    assert_eq!(app.unresolved_count, 1);
    app.close_capture();
    assert_eq!(app.unresolved_count, 1);
    app.switch_device("fixture".to_owned(), &ctx);
    app.update_processes(HashMap::from([(9999, "com.example.other".to_owned())]));
    assert!(
        app.entries[0].process.is_none(),
        "old PID must not acquire a new identity"
    );
    app.package.clear();
    app.schedule_filter();
    settle(&mut app);
    assert_eq!(app.matches, [0]);
    app.clear_logs();
    assert_eq!(app.unresolved_count, 0);
}

#[test]
fn resolved_rows_keep_identity_but_new_rows_use_current_snapshot() {
    let (_, mut app) = app();
    app.append_entry(row());
    app.update_processes(HashMap::from([(9999, "com.example.alpha".to_owned())]));
    assert_eq!(app.unresolved_count, 0);
    app.update_processes(HashMap::new());
    app.append_entry(row());
    assert_eq!(app.entries[0].process.as_deref(), Some("com.example.alpha"));
    assert!(app.entries[1].process.is_none());
    assert_eq!(app.unresolved_count, 1);
}

#[test]
fn selected_level_text_contrast_is_readable_in_both_themes() {
    fn luminance(color: Color32) -> f64 {
        let c = color.to_array();
        let linear = |v: u8| {
            let s = f64::from(v) / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2])
    }
    for dark in [true, false] {
        let visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        for level in Level::ALL {
            let fg = luminance(selected_level_color(level, dark, true));
            let bg = luminance(visuals.selection.bg_fill);
            let contrast = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
            assert!(contrast >= 4.5, "{level:?} dark={dark}: {contrast}");
            assert_eq!(selected_level_color(level, dark, false), level.color(dark));
        }
    }
}

#[test]
fn save_dialog_keyboard_cancel_and_blank_enter_do_not_save() {
    let (ctx, mut app) = app();
    app.show_save_filter = true;
    frame(&mut app, &ctx, None);
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Enter, egui::Modifiers::NONE)),
    );
    assert!(app.show_save_filter);
    assert!(app.saved_filters.is_empty());
    app.save_filter_name = "cancelled".to_owned();
    ctx.memory_mut(|m| m.request_focus(egui::Id::new("save_filter_name")));
    frame(&mut app, &ctx, None);
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Tab, egui::Modifiers::NONE)),
    );
    frame(&mut app, &ctx, None);
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Tab, egui::Modifiers::NONE)),
    );
    frame(&mut app, &ctx, None);
    frame(
        &mut app,
        &ctx,
        Some((egui::Key::Enter, egui::Modifiers::NONE)),
    );
    assert!(!app.show_save_filter);
    assert!(app.saved_filters.is_empty());
}

#[test]
fn old_process_query_cannot_assign_identity_after_new_capture_epoch() {
    let (ctx, mut app) = app();
    app.switch_device("fixture".to_owned(), &ctx);
    let old = app.capture_session_id;
    app.pause_capture();
    app.append_entry(row());
    app.backend_tx
        .send(BackendEvent::Processes {
            serial: "fixture".to_owned(),
            session_id: old,
            result: Ok(HashMap::from([(9999, "stale.package".to_owned())])),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.entries[0].process.is_none());
    assert!(app.processes.is_empty());
    assert_eq!(app.unresolved_count, 1);
}

#[test]
fn trim_keeps_unknown_count_and_rebases_only_live_unresolved_indices() {
    let (ctx, mut app) = app();
    app.switch_device("fixture".to_owned(), &ctx);
    for _ in 0..300 {
        app.append_entry(LogEntry::new(
            "",
            9999,
            9999,
            "Old",
            Level::Fatal,
            "x".repeat(16384),
        ));
    }
    app.close_capture();
    app.switch_device("fixture".to_owned(), &ctx);
    app.memory_limit_mib = 16;
    for i in 0..800 {
        app.append_entry(LogEntry::new(
            "",
            42,
            42,
            "Tag",
            Level::Info,
            format!("{i} {}", "x".repeat(16384)),
        ));
    }
    assert!(app.dropped_entries > 0);
    assert_eq!(app.unresolved_count, app.entries.len());
    app.update_processes(HashMap::from([
        (42, "new.package".to_owned()),
        (9999, "reused.pid".to_owned()),
    ]));
    assert_eq!(
        app.unresolved_count,
        app.entries.iter().filter(|e| e.process.is_none()).count()
    );
    assert!(
        app.entries.iter().any(|e| e.pid == 9999),
        "must retain old unresolved rows after trim"
    );
    assert!(
        app.entries.iter().any(|e| e.pid == 42),
        "must retain live unresolved rows after trim"
    );
    for e in app.entries.iter() {
        if e.pid == 42 {
            assert_eq!(e.process.as_deref(), Some("new.package"));
        } else {
            assert!(e.process.is_none());
        }
    }
}

#[test]
fn newer_device_selection_or_close_cancels_old_destructive_intent() {
    let (ctx, mut app) = app();
    app.switch_device("first".to_owned(), &ctx);
    app.append_entry(row());
    app.request_device_switch("second".to_owned(), &ctx);
    app.request_device_switch("first".to_owned(), &ctx);
    assert!(app.pending_device.is_none());
    assert_eq!(app.entries.len(), 1);
    app.pending_clear = true;
    app.close_capture();
    assert!(!app.pending_clear);
    assert_eq!(app.entries.len(), 1);
    app.pending_clear = true;
    app.show_save_filter = true;
    frame(&mut app, &ctx, None);
    assert!(!app.show_save_filter);
    assert!(app.pending_clear);
}

#[test]
fn clear_button_cancel_confirm_and_device_confirm_use_real_input() {
    let (ctx, app) = app();
    let mut h = super::ui_test_support::Harness::from_app(ctx, app, egui::vec2(900.0, 560.0));
    h.app.switch_device("first".to_owned(), &h.ctx);
    h.app.append_entry(row());
    h.click_rendered_label("Clear");
    assert!(h.app.pending_clear);
    assert_eq!(h.app.entries.len(), 1);
    h.click_rendered_label("Cancel");
    assert!(!h.app.pending_clear);
    assert_eq!(h.app.entries.len(), 1);
    h.click_rendered_label("Clear");
    h.click_rendered_label("清空并继续");
    assert!(h.app.entries.is_empty());
    assert!(!h.app.pending_clear);
    h.app.append_entry(row());
    h.app.request_device_switch("second".to_owned(), &h.ctx);
    h.click_rendered_label("清空并继续");
    assert_eq!(h.app.selected_device.as_deref(), Some("second"));
    assert!(h.app.entries.is_empty());
}

#[test]
fn unknown_notice_action_preserves_other_filters_and_reveals_crash() {
    let (ctx, app) = app();
    let mut h = super::ui_test_support::Harness::from_app(ctx, app, egui::vec2(900.0, 560.0));
    h.app.append_entry(row());
    h.app.package = "com.example.alpha".to_owned();
    h.app.query = "FATAL".to_owned();
    h.app.min_level = Level::Error;
    h.app.schedule_filter();
    settle(&mut h.app);
    assert!(h.app.matches.is_empty());
    h.click_rendered_label("清除包名限制以查看");
    settle(&mut h.app);
    assert!(h.app.package.is_empty());
    assert_eq!(h.app.query, "FATAL");
    assert_eq!(h.app.min_level, Level::Error);
    assert_eq!(h.app.matches, [0]);
}
