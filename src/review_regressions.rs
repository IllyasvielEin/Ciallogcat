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
    assert_eq!(app.selected_device.as_deref(), Some("fixture"));
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
    app.update_processes(HashMap::from([(9999, "com.example.alpha".to_owned())]));
    app.append_entry(row());
    assert_eq!(app.unresolved_count, 0);
    app.update_processes(HashMap::new());
    app.append_entry(row());
    assert_eq!(app.entries[0].process.as_deref(), Some("com.example.alpha"));
    assert!(app.entries[1].process.is_none());
    assert_eq!(app.unresolved_count, 1);
}

#[test]
fn same_session_pid_reuse_never_claims_unknown_history() {
    let (_, mut app) = app();
    let session = app.capture_session_id;
    app.append_entry(row());
    app.update_processes(HashMap::from([(9999, "com.example.alpha".to_owned())]));
    app.append_entry(row());
    app.update_processes(HashMap::new());
    app.append_entry(row());
    app.update_processes(HashMap::from([(9999, "com.example.beta".to_owned())]));
    app.append_entry(row());

    assert_eq!(app.capture_session_id, session);
    assert_eq!(app.unresolved_count, 2);
    assert_eq!(app.entries.unresolved_count(), 2);
    for index in [0, 2] {
        assert!(app.entries[index].process.is_none());
        assert!(app.entries[index].process_source.is_none());
    }
    for (index, process) in [(1, "com.example.alpha"), (3, "com.example.beta")] {
        assert_eq!(app.entries[index].process.as_deref(), Some(process));
        assert_eq!(
            app.entries[index].process_source,
            Some(ProcessSource::PsSnapshot)
        );
    }
}

#[test]
fn queued_logs_do_not_acquire_a_later_process_sample() {
    let (ctx, mut app) = app();
    app.begin_test_capture();
    let queued = adb::QueuedLogEntry::from(row());
    let received_at = queued.received_at();
    while Instant::now() <= received_at {
        std::thread::yield_now();
    }
    let completed = Instant::now();
    app.update_process_snapshot(
        HashMap::from([(9999, "com.example.later".to_owned())]),
        received_at,
        completed,
    );
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: app.capture_session_id,
            event: LogcatEvent::Entry(queued),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.entries[0].process.is_none());

    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: app.capture_session_id,
            event: LogcatEvent::Entry(row().into()),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.entries[1].process.as_deref(), Some("com.example.later"));
}

#[test]
fn delayed_expired_and_failed_process_samples_leave_new_logs_unknown() {
    let (ctx, mut app) = app();
    app.begin_test_capture();
    let now = Instant::now();
    app.update_process_snapshot(
        HashMap::from([(9999, "com.example.delayed".to_owned())]),
        now - PROCESS_HINT_TTL - Duration::from_millis(1),
        now,
    );
    app.append_entry(row());
    assert!(app.entries[0].process.is_none());
    assert!(app.processes.is_empty());

    app.update_processes(HashMap::from([(9999, "com.example.expired".to_owned())]));
    app.expire_processes(Instant::now() + PROCESS_HINT_TTL + Duration::from_millis(1));
    app.append_entry(row());
    assert!(app.entries[1].process.is_none());

    app.update_processes(HashMap::from([(9999, "com.example.sampled".to_owned())]));
    app.append_entry(row());
    app.backend_tx
        .send(BackendEvent::Processes {
            serial: "fixture".to_owned(),
            session_id: app.capture_session_id,
            observed_from: Instant::now(),
            observed_until: Instant::now(),
            result: Err("ps timed out".to_owned()),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.process_query_error.as_deref(), Some("ps timed out"));
    assert!(app.processes.is_empty());
    app.append_entry(row());
    assert!(app.entries[3].process.is_none());
    assert_eq!(
        app.entries[2].process.as_deref(),
        Some("com.example.sampled")
    );
    assert_eq!(app.unresolved_count, 3);
}

#[cfg(unix)]
struct StreamScript(std::path::PathBuf);

#[cfg(unix)]
impl Drop for StreamScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn stream_script() -> StreamScript {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "ciallogcat-discovery-{}-{}.sh",
        std::process::id(),
        NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed),
    ));
    let script = StreamScript(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&script.0)
        .unwrap();
    file.write_all(
        b"#!/bin/sh\nif [ \"$3\" = shell ]; then exit 0; fi\nprintf '10-23 12:00:00.000 7 7 I Fixture: stream alive %s\\n' \"$*\"\nexec sleep 30\n",
    )
    .unwrap();
    drop(file);
    script
}

#[cfg(unix)]
#[test]
fn discovery_failures_preserve_a_live_stream_and_user_can_still_stop_it() {
    let script = stream_script();

    let (ctx, mut app) = app();
    let online = DeviceInfo {
        serial: "fixture".to_owned(),
        state: "device".to_owned(),
        model: None,
        transport: "USB",
    };
    app.devices = vec![online.clone()];
    app.selected_device = Some("fixture".to_owned());
    app.retained_device = app.selected_device.clone();
    let session = app.begin_test_capture();
    app.capture_handle = Some(
        adb::start_logcat(
            &script.0,
            "fixture",
            &app.buffers,
            session,
            app.backend_tx.clone(),
            wake_signal(&ctx),
        )
        .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.entries.is_empty() {
        app.poll_backend(&ctx);
        assert!(Instant::now() < deadline, "fixture stream did not start");
        std::thread::yield_now();
    }
    assert!(matches!(app.capture_state, CaptureState::Running));
    app.backend_tx
        .send(BackendEvent::Devices(Err("discovery timed out".to_owned())))
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.devices.as_slice(), std::slice::from_ref(&online));
    assert_eq!(
        app.device_query_error.as_deref(),
        Some("discovery timed out")
    );
    assert!(app.capture_handle.is_some());
    assert_eq!(app.capture_session_id, session);

    for devices in [
        vec![DeviceInfo {
            state: "offline".to_owned(),
            ..online.clone()
        }],
        vec![],
    ] {
        app.backend_tx
            .send(BackendEvent::Devices(Ok(devices)))
            .unwrap();
        app.poll_backend(&ctx);
        assert!(app.device_query_error.is_none());
        assert_eq!(app.selected_device.as_deref(), Some("fixture"));
        assert_eq!(app.capture_session_id, session);
        assert!(app.capture_handle.is_some());
        assert!(matches!(app.capture_state, CaptureState::Running));
    }
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: session,
            event: LogcatEvent::Entry(row().into()),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.entries.len(), 2);

    let mut harness = ui_test_support::Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    harness.click_label("暂停采集");
    assert!(harness.app.capture_handle.is_some());
    assert!(matches!(harness.app.capture_state, CaptureState::Stopping));
    let deadline = Instant::now() + Duration::from_secs(3);
    while harness.app.capture_handle.is_some() {
        harness.app.poll_backend(&harness.ctx);
        assert!(Instant::now() < deadline, "stop confirmation missing");
        std::thread::yield_now();
    }
    assert!(matches!(harness.app.capture_state, CaptureState::Paused));
    harness
        .app
        .backend_tx
        .send(BackendEvent::Devices(Ok(vec![online])))
        .unwrap();
    harness
        .app
        .backend_tx
        .send(BackendEvent::Logcat {
            session_id: session,
            event: LogcatEvent::Entry(row().into()),
        })
        .unwrap();
    harness.app.poll_backend(&harness.ctx);
    assert!(harness.app.capture_handle.is_none());
    assert!(matches!(harness.app.capture_state, CaptureState::Paused));
    assert_eq!(harness.app.entries.len(), 2);
}

#[cfg(unix)]
#[test]
fn buffer_restart_waits_for_missing_device_then_resumes_with_new_buffers() {
    let script = stream_script();
    let (ctx, mut app) = app();
    app.adb_path = Some(script.0.clone());
    let online = DeviceInfo {
        serial: "fixture".to_owned(),
        state: "device".to_owned(),
        model: None,
        transport: "USB",
    };
    app.devices = vec![online.clone()];
    app.selected_device = Some("fixture".to_owned());
    app.retained_device = app.selected_device.clone();
    // This fixture serves only Logcat. Drive discovery explicitly below.
    let poll = |app: &mut CiallogcatApp| {
        app.last_device_query = Instant::now();
        app.last_process_query = Instant::now();
        app.poll_backend(&ctx);
    };
    app.start_capture(&ctx);
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.entries.is_empty() {
        poll(&mut app);
        assert!(Instant::now() < deadline, "first stream did not start");
        std::thread::yield_now();
    }
    let first_session = app.capture_session_id;
    assert!(matches!(app.capture_state, CaptureState::Running));
    app.backend_tx
        .send(BackendEvent::Devices(Ok(vec![])))
        .unwrap();
    poll(&mut app);
    assert!(app.capture_handle.is_some());

    app.buffer_draft = vec![LogBuffer::Events];
    app.apply_buffers(&ctx);
    assert_eq!(app.buffers, [LogBuffer::Events]);
    assert!(app.capture_handle.is_some());
    assert!(matches!(app.capture_state, CaptureState::Stopping));
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.capture_handle.is_some() {
        poll(&mut app);
        assert!(
            Instant::now() < deadline,
            "old stream was not confirmed stopped"
        );
        std::thread::yield_now();
    }
    assert!(matches!(app.capture_state, CaptureState::WaitingForDevice));
    assert_eq!(app.selected_device.as_deref(), Some("fixture"));
    assert_eq!(app.entries.len(), 1);

    app.backend_tx
        .send(BackendEvent::Devices(Ok(vec![online])))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.entries.len() < 2 {
        poll(&mut app);
        assert!(
            Instant::now() < deadline,
            "replacement stream did not start"
        );
        std::thread::yield_now();
    }
    assert!(app.capture_handle.is_some());
    assert_ne!(app.capture_session_id, first_session);
    assert!(matches!(app.capture_state, CaptureState::Running));
    assert!(app.entries[1].message.contains("-b events"));
    assert!(!app.entries[1].message.contains("-b main"));
    app.pause_capture();
}

#[cfg(unix)]
#[test]
fn stopped_stream_drains_before_restart_and_source_changes_reject_old_data() {
    fn wait_for(
        app: &mut CiallogcatApp,
        ctx: &egui::Context,
        done: impl Fn(&CiallogcatApp) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done(app) {
            app.last_device_query = Instant::now();
            app.last_process_query = Instant::now();
            app.poll_backend(ctx);
            assert!(
                Instant::now() < deadline,
                "capture transition did not settle"
            );
            std::thread::yield_now();
        }
    }
    let script = stream_script();
    let (ctx, mut app) = app();
    app.adb_path = Some(script.0.clone());
    app.devices = ["first", "second"]
        .into_iter()
        .map(|serial| DeviceInfo {
            serial: serial.to_owned(),
            state: "device".to_owned(),
            model: None,
            transport: "USB",
        })
        .collect();
    app.switch_device("first".to_owned(), &ctx);
    wait_for(&mut app, &ctx, |app| app.entries.len() == 1);
    let original = app.entries[0].provenance.unwrap();
    let old = app.capture_session_id;
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: old,
            event: LogcatEvent::Entry(LogEntry::marker("already queued before pause").into()),
        })
        .unwrap();
    app.pause_capture();
    assert!(app.capture_handle.is_some());
    assert!(matches!(app.capture_state, CaptureState::Stopping));
    wait_for(&mut app, &ctx, |app| app.capture_handle.is_none());
    assert_eq!(app.entries.len(), 2);
    assert_eq!(app.entries[1].provenance.unwrap().seq, 2);
    assert!(matches!(app.capture_state, CaptureState::Paused));

    app.start_capture(&ctx);
    wait_for(&mut app, &ctx, |app| app.entries.len() == 3);
    let resumed = app.entries[2].provenance.unwrap();
    assert_eq!(resumed.session_id, original.session_id);
    assert_eq!((resumed.seq, resumed.segment, resumed.attempt), (3, 2, 2));
    app.clear_logs();
    assert_eq!(app.live_session.last_seq(), 3);
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: app.capture_session_id,
            event: LogcatEvent::Entry(row().into()),
        })
        .unwrap();
    wait_for(&mut app, &ctx, |app| app.entries.len() == 1);
    assert_eq!(app.entries[0].provenance.unwrap().seq, 4);
    app.close_capture();
    assert!(app.capture_handle.is_some());
    wait_for(&mut app, &ctx, |app| app.capture_handle.is_none());
    app.switch_device("first".to_owned(), &ctx);
    wait_for(&mut app, &ctx, |app| app.entries.len() == 2);
    assert_eq!(
        app.entries[1].provenance.unwrap().session_id,
        original.session_id
    );
    assert_eq!(app.entries[1].provenance.unwrap().seq, 5);

    let replaced = app.capture_session_id;
    app.switch_device("second".to_owned(), &ctx);
    assert!(app.entries.is_empty());
    assert!(
        app.capture_handle.is_some(),
        "old handle retained until Exited"
    );
    assert_eq!(
        app.capture_session_id, replaced,
        "replacement is not spawned yet"
    );
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: replaced,
            event: LogcatEvent::Entry(LogEntry::marker("old source must not leak").into()),
        })
        .unwrap();
    wait_for(&mut app, &ctx, |app| !app.entries.is_empty());
    assert_eq!(app.entries.len(), 1);
    assert!(app.entries[0].message.contains("-s second"));
    let switched = app.entries[0].provenance.unwrap();
    assert_ne!(switched.session_id, original.session_id);
    assert_eq!(
        (switched.seq, switched.segment, switched.attempt),
        (1, 1, 1)
    );
    app.close_capture();
    wait_for(&mut app, &ctx, |app| app.capture_handle.is_none());
}

#[test]
fn query_errors_clear_only_for_their_source_and_do_not_follow_device_switches() {
    let (ctx, mut app) = app();
    app.selected_device = Some("first".to_owned());
    app.device_query_error = Some("discovery failed".to_owned());
    app.process_query_error = Some("ps failed".to_owned());
    app.backend_tx
        .send(BackendEvent::Packages {
            serial: "first".to_owned(),
            result: Err("packages failed".to_owned()),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.package_query_error.as_deref(), Some("packages failed"));
    app.backend_tx
        .send(BackendEvent::Devices(Ok(vec![])))
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.device_query_error.is_none());
    assert_eq!(app.package_query_error.as_deref(), Some("packages failed"));
    app.backend_tx
        .send(BackendEvent::Packages {
            serial: "first".to_owned(),
            result: Ok(vec!["com.example.app".to_owned()]),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.package_query_error.is_none());
    assert_eq!(app.packages, ["com.example.app"]);
    assert_eq!(app.process_query_error.as_deref(), Some("ps failed"));

    app.package_query_error = Some("first package error".to_owned());
    app.switch_device("second".to_owned(), &ctx);
    assert!(app.package_query_error.is_none());
    assert!(app.process_query_error.is_none());
    app.backend_tx
        .send(BackendEvent::Packages {
            serial: "first".to_owned(),
            result: Err("old device failed".to_owned()),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.package_query_error.is_none());
    app.package_query_error = Some("second package error".to_owned());
    app.process_query_error = Some("second process error".to_owned());
    app.close_capture();
    assert!(app.package_query_error.is_none());
    assert!(app.process_query_error.is_none());
}

#[test]
fn returning_to_same_serial_before_stop_ack_does_not_reassign_old_stream() {
    let (ctx, mut app) = app();
    app.selected_device = Some("first".to_owned());
    let old = app.begin_test_capture();
    let old_document = app.live_session.id();
    app.switch_device("second".to_owned(), &ctx);
    app.switch_device("first".to_owned(), &ctx);
    let new_document = app.live_session.id();
    assert_ne!(old_document, new_document);
    assert_eq!(app.capture_controller.active_id(), Some(old));
    assert!(app.capture_controller.is_stopping());
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: old,
            event: LogcatEvent::Entry(LogEntry::marker("old first stream").into()),
        })
        .unwrap();
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: old,
            event: LogcatEvent::Exited {
                code: Some(0),
                error: None,
                incomplete_proto_frame: false,
            },
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.entries.is_empty());
    assert_eq!(app.live_session.last_seq(), 0);
    assert!(app.capture_controller.active_id().is_none());

    let new = app.begin_test_capture();
    assert_ne!(new, old);
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: old,
            event: LogcatEvent::Exited {
                code: Some(0),
                error: None,
                incomplete_proto_frame: false,
            },
        })
        .unwrap();
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: new,
            event: LogcatEvent::Entry(row().into()),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.capture_controller.active_id(), Some(new));
    assert_eq!(app.entries.len(), 1);
    let identity = app.entries[0].provenance.unwrap();
    assert_eq!(identity.session_id, new_document);
    assert_eq!(
        (identity.seq, identity.segment, identity.attempt),
        (1, 1, 1)
    );
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
            observed_from: Instant::now(),
            observed_until: Instant::now(),
            result: Ok(HashMap::from([(9999, "stale.package".to_owned())])),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert!(app.entries[0].process.is_none());
    assert!(app.processes.is_empty());
    assert_eq!(app.unresolved_count, 1);
}

#[test]
fn trim_keeps_unknown_count_without_backfilling_any_old_pid() {
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
        app.entries
            .iter()
            .filter(|e| e.is_unresolved_process())
            .count()
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
        assert!(e.process.is_none());
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
    h.click_rendered_label("清空视图");
    assert!(h.app.pending_clear);
    assert_eq!(h.app.entries.len(), 1);
    h.click_rendered_label("Cancel");
    assert!(!h.app.pending_clear);
    assert_eq!(h.app.entries.len(), 1);
    h.click_rendered_label("清空视图");
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

#[test]
fn gui_deadlines_cover_real_work_and_leave_idle_without_a_periodic_pump() {
    let (ctx, mut app) = app();
    let now = Instant::now();
    assert_eq!(app.next_wake_deadline(now), None);
    app.filter_dirty_since = Some(now);
    app.config_dirty_since = Some(now);
    assert_eq!(app.next_wake_deadline(now), Some(now + FILTER_DEBOUNCE));
    app.filter_dirty_since = None;
    assert_eq!(app.next_wake_deadline(now), Some(now + CONFIG_SAVE_DELAY));
    app.config_dirty_since = None;
    app.adb_path = Some("unused-adb".into());
    app.last_device_query = now;
    assert_eq!(
        app.next_wake_deadline(now),
        Some(now + DEVICE_POLL_INTERVAL)
    );
    app.device_query_pending = true;
    assert_eq!(
        app.next_wake_deadline(now),
        None,
        "query completion owns its wake"
    );
    app.adb_path = None;
    app.filter_pending = true;
    app.filter_progress = None;
    assert_eq!(
        app.next_wake_deadline(now),
        None,
        "background scan owns its wake"
    );
    app.filter_pending = false;
    app.process_snapshot = Some((now - PROCESS_HINT_TTL, now));
    app.logs_per_second = 3.0;
    app.rate_window_started = now - Duration::from_secs(1);
    assert!(app.next_wake_deadline(now).unwrap() <= now);
    app.poll_backend(&ctx);
    assert!(app.process_snapshot.is_none());
    assert_eq!(app.logs_per_second, 0.0);
    assert_eq!(
        app.next_wake_deadline(Instant::now()),
        None,
        "completed deadline must not spin"
    );
}

#[test]
fn filter_and_detail_workers_wake_after_publishing_results() {
    let (_, mut app) = app();
    app.append_entry(row());
    let (tx, rx) = mpsc::channel();
    let wake = WakeSignal::new(move || {
        let _ = tx.send(());
    });
    let (sender, receiver) = filter::spawn_filter_worker(wake.clone());
    sender.send(FilterRequest {
        generation: 99,
        log_epoch: 1,
        snapshot_base: 0,
        entries: Arc::clone(&app.entries),
        spec: app.current_filter(),
    });
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(receiver.take_filter().unwrap().generation, 99);
    let worker = reading::DetailWorker::new(wake);
    let generation = worker.submit(Arc::from("a hidden needle"), Regex::new("needle").unwrap());
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let result = worker.take().unwrap();
    assert_eq!(result.generation, generation);
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0], 9..15);
}

#[test]
fn retry_becoming_due_during_layout_wakes_once_but_missing_device_never_spins() {
    let (_, mut app) = app();
    let id = app.begin_test_capture();
    let now = Instant::now();
    app.capture_controller
        .exited(id, "fixture exit".into(), now);
    app.adb_path = Some("unused-adb".into());
    app.device_query_pending = true; // Discovery worker owns its next wake.
    app.devices = vec![DeviceInfo {
        serial: "fixture".into(),
        state: "device".into(),
        model: None,
        transport: "USB",
    }];
    let due = now + Duration::from_secs(1);
    assert_eq!(
        app.next_wake_deadline(due - Duration::from_nanos(1)),
        Some(due)
    );
    assert_eq!(app.next_wake_deadline(due), Some(due));
    app.devices.clear();
    assert_eq!(app.next_wake_deadline(due), None);
    app.pause_capture();
    assert_eq!(app.next_wake_deadline(due), None);
}
