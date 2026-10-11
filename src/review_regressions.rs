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
