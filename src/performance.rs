//! Reproducible CPU-side UI measurements; no GPU, ADB, or user configuration writes.
//! Run: cargo test --release performance::measure_ui -- --ignored --nocapture
use super::*;

fn sample(index: usize, message: &str) -> LogEntry {
    let mut entry = LogEntry::new(
        "09-06 12:34:56.789",
        1234,
        1235,
        "NetworkClient",
        Level::ALL[index % 6],
        format!("{index}: {message}"),
    );
    entry.process = Some(Arc::from("com.example.app:worker"));
    entry
}

fn frame(app: &mut CiallogcatApp, ctx: &egui::Context, width: f32) {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 820.0),
            )),
            ..Default::default()
        },
        |ui| {
            app.toolbar(ui);
            app.status_bar(ui);
            app.details(ui);
            app.log_table(ui);
        },
    );
    std::hint::black_box(ctx.tessellate(output.shapes, output.pixels_per_point));
    output.textures_delta.clear();
}

fn report(name: &str, mut times: Vec<f64>) {
    times.sort_by(f64::total_cmp);
    println!(
        "{name}: p50={:.3}ms p95={:.3}ms max={:.3}ms",
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() - 1]
    );
}

#[test]
#[ignore = "manual release performance measurement"]
fn measure_ui() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    let message = "请求完成 request timeout failed code=500 elapsed=123ms";
    for index in 0..200_000 {
        app.append_entry(sample(index, message));
    }
    app.query = "timeout|failed".to_owned();
    app.use_regex = true;
    for width in [1280.0, 900.0] {
        let mut times = Vec::new();
        for index in 0..70 {
            let started = Instant::now();
            frame(&mut app, &ctx, width);
            if index >= 10 {
                times.push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        report(&format!("200k rows UI+tessellation width={width}"), times);
    }
    // A held filter snapshot must stay immutable as capture continues.
    app.memory_limit_mib = 64;
    let mut times = Vec::new();
    for index in 0..30 {
        let snapshot = Arc::clone(&app.entries);
        let started = Instant::now();
        app.append_entry(sample(index, message));
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(snapshot);
    }
    report(
        "append with live snapshot (first sample includes trim)",
        times,
    );

    let spec = app.current_filter();
    let matcher = filter::compile_matcher(&spec).unwrap();
    let started = Instant::now();
    let count = app
        .entries
        .iter()
        .filter(|entry| filter::entry_matches(entry, &spec, matcher.as_ref()))
        .count();
    println!(
        "filter {} rows: {:.3}ms ({count} matches)",
        app.entries.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );

    // Extremely long single lines still need a responsive table preview.
    app.clear_logs();
    let long_message = message.repeat(2000);
    for index in 0..40 {
        app.append_entry(sample(index, &long_message));
    }
    let mut times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        frame(&mut app, &ctx, 1280.0);
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    report("40 rows with ~120KB messages", times);
    app.selected = Some(0);
    let mut times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        frame(&mut app, &ctx, 1280.0);
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    report("long message selected", times);

    app.clear_logs();
    for index in 0..180_000 {
        app.append_entry(sample(index, message));
    }
    let tx = app.backend_tx.clone();
    let session_id = app.capture_session_id;
    let producer = std::thread::spawn(move || {
        for index in 180_000..230_000 {
            tx.send(BackendEvent::Logcat {
                session_id,
                event: LogcatEvent::Entry(sample(index, message).into()),
            })
            .unwrap();
        }
    });
    let mut frame_times = Vec::new();
    let mut receive_times = Vec::new();
    while app.entries.len() + app.dropped_entries < 230_000 {
        let started = Instant::now();
        app.poll_filter();
        if frame_times.len() % 10 == 0 {
            app.schedule_filter();
        }
        let receive_started = Instant::now();
        app.poll_backend(&ctx);
        receive_times.push(receive_started.elapsed().as_secs_f64() * 1000.0);
        frame(&mut app, &ctx, 1280.0);
        frame_times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    producer.join().unwrap();
    report(
        "50k event burst over 180k history: receive slice",
        receive_times,
    );
    report("50k event burst + refilter + UI+tessellation", frame_times);
    assert!(app.entries.memory_bytes() <= app.memory_limit_mib * MIB);
}

#[test]
fn backend_drain_yields_and_preserves_queued_logs() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    for index in 0..BACKEND_QUEUE_CAPACITY {
        app.backend_tx
            .try_send(BackendEvent::Logcat {
                session_id: app.capture_session_id,
                event: LogcatEvent::Entry(sample(index, "ready").into()),
            })
            .unwrap();
    }
    app.poll_backend(&ctx);
    assert!(app.entries.len() <= BACKEND_EVENTS_PER_FRAME);
    assert!(app.entries.len() < BACKEND_QUEUE_CAPACITY);
    while app.entries.len() < BACKEND_QUEUE_CAPACITY {
        app.poll_backend(&ctx);
    }
    assert_eq!(app.matches.len(), BACKEND_QUEUE_CAPACITY);
    assert!(
        app.entries[BACKEND_QUEUE_CAPACITY - 1]
            .message
            .starts_with("4095:")
    );
}

#[test]
fn navigation_follows_filtered_rows_and_bounds() {
    let rows = [2, 7, 20];
    assert_eq!(
        navigated_selection(&rows, None, egui::Key::ArrowDown),
        Some(2)
    );
    assert_eq!(
        navigated_selection(&rows, Some(2), egui::Key::ArrowUp),
        Some(2)
    );
    assert_eq!(
        navigated_selection(&rows, Some(7), egui::Key::ArrowDown),
        Some(20)
    );
    assert_eq!(
        navigated_selection(&rows, Some(20), egui::Key::ArrowDown),
        Some(20)
    );
    assert_eq!(
        navigated_selection(&rows, Some(7), egui::Key::Home),
        Some(2)
    );
    assert_eq!(
        navigated_selection(&rows, Some(7), egui::Key::End),
        Some(20)
    );
    assert_eq!(navigated_selection(&[], Some(7), egui::Key::ArrowUp), None);
}

#[test]
fn long_unicode_messages_keep_complete_copy_and_valid_segments() {
    let text = "请求🦀完成\n".repeat(10_000);
    let entry = sample(0, &text);
    assert!(preview_message(&text).len() <= 2048);
    let reconstructed: String = detail_ranges(&text)
        .iter()
        .map(|range| &text[range.clone()])
        .collect();
    assert_eq!(reconstructed, text);
    assert!(entry.copy_text().ends_with(&text));
}

fn key_frame(
    app: &mut CiallogcatApp,
    ctx: &egui::Context,
    key: egui::Key,
    modifiers: egui::Modifiers,
) -> egui::FullOutput {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 560.0),
            )),
            events: vec![if key == egui::Key::C && modifiers.command {
                egui::Event::Copy
            } else {
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }
            }],
            ..Default::default()
        },
        |ui| {
            app.shortcuts(ctx);
            app.toolbar(ui);
            app.status_bar(ui);
            app.details(ui);
            app.log_table(ui);
            app.copy_shortcut(ctx);
        },
    );
    output.textures_delta.clear();
    output
}

#[test]
fn keyboard_find_preserves_query_and_navigation_respects_input_focus() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    for index in 0..3 {
        app.append_entry(sample(index, "message"));
    }
    app.query = "message".to_owned();
    frame(&mut app, &ctx, 900.0);
    key_frame(&mut app, &ctx, egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(app.selected, Some(0));
    key_frame(&mut app, &ctx, egui::Key::F, egui::Modifiers::COMMAND);
    assert!(ctx.text_edit_focused());
    frame(&mut app, &ctx, 900.0);
    key_frame(&mut app, &ctx, egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(app.selected, Some(0));
    assert!(
        ctx.text_edit_focused(),
        "query must remain focused before Escape"
    );
    key_frame(&mut app, &ctx, egui::Key::Escape, egui::Modifiers::NONE);
    assert_eq!(app.query, "message");
    assert_eq!(app.selected, Some(0));
    key_frame(&mut app, &ctx, egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(app.selected, Some(1));
    let output = key_frame(&mut app, &ctx, egui::Key::C, egui::Modifiers::COMMAND);
    assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == &app.entries[1].copy_text())));
}

#[test]
fn filtering_during_capture_and_trim_keeps_valid_indices() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    for index in 0..200_000 {
        app.append_entry(sample(index, "message"));
    }
    app.query = "message".to_owned();
    app.schedule_filter();
    app.memory_limit_mib = 16;
    for index in 0..10 {
        app.append_entry(sample(index, "message"));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.filter_pending && Instant::now() < deadline {
        app.poll_filter();
        std::thread::yield_now();
    }
    assert!(!app.filter_pending);
    assert!(app.dropped_entries > 0);
    assert!(app.entries.memory_bytes() <= 16 * MIB);
    assert_eq!(app.matches.len(), app.entries.len());
    assert!(app.matches.iter().copied().eq(0..app.entries.len()));
    app.schedule_filter();
    app.clear_logs();
    app.append_entry(sample(0, "message"));
    std::thread::sleep(Duration::from_millis(30));
    app.poll_filter();
    assert_eq!(app.matches, [0]);
    assert_eq!(app.entries.len(), 1);
}

#[test]
fn pause_and_close_ignore_old_session_events_and_keep_logs() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.selected_device = Some("fixture".to_owned());
    app.append_entry(sample(0, "retained"));
    let old_session = app.capture_session_id;
    app.pause_capture();
    assert!(matches!(app.capture_state, CaptureState::Paused));
    assert_eq!(app.selected_device.as_deref(), Some("fixture"));
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: old_session,
            event: LogcatEvent::Entry(sample(1, "stale").into()),
        })
        .unwrap();
    // Paused state does not auto-start when periodic device discovery succeeds.
    app.devices.push(DeviceInfo {
        serial: "fixture".to_owned(),
        state: "device".to_owned(),
        model: None,
        transport: "USB",
    });
    app.reconcile_device_selection(&ctx);
    app.poll_backend(&ctx);
    assert!(matches!(app.capture_state, CaptureState::Paused));
    assert_eq!(app.entries.len(), 1);
    app.buffer_draft = vec![LogBuffer::Crash];
    app.apply_buffers(&ctx);
    assert_eq!(app.buffers, [LogBuffer::Crash]);
    assert!(matches!(app.capture_state, CaptureState::Paused));
    app.close_capture();
    assert!(app.selected_device.is_none());
    assert_eq!(app.entries.len(), 1);
}

#[test]
fn multi_selection_shortcuts_copy_only_filtered_rows_and_respect_text_focus() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    for index in 0..5 {
        app.append_entry(sample(index, "message\ncontinuation"));
    }
    app.matches = vec![0, 2, 4];
    frame(&mut app, &ctx, 900.0);
    key_frame(&mut app, &ctx, egui::Key::Home, egui::Modifiers::NONE);
    key_frame(&mut app, &ctx, egui::Key::ArrowDown, egui::Modifiers::SHIFT);
    let output = key_frame(&mut app, &ctx, egui::Key::C, egui::Modifiers::COMMAND);
    let expected = format!(
        "{}\n{}",
        app.entries[0].copy_text(),
        app.entries[2].copy_text()
    );
    assert!(output.platform_output.commands.iter().any(
        |command| matches!(command, egui::OutputCommand::CopyText(text) if text == &expected)
    ));
    key_frame(&mut app, &ctx, egui::Key::A, egui::Modifiers::COMMAND);
    assert!(app.selection.contains(4));
    assert!(!app.selection.contains(1));
    key_frame(&mut app, &ctx, egui::Key::Escape, egui::Modifiers::NONE);
    assert!(app.selection.is_empty());
    key_frame(&mut app, &ctx, egui::Key::F, egui::Modifiers::COMMAND);
    frame(&mut app, &ctx, 900.0);
    key_frame(&mut app, &ctx, egui::Key::A, egui::Modifiers::COMMAND);
    assert!(app.selection.is_empty());
}

#[test]
fn selecting_during_capture_stops_following_until_explicit_resume() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.show_details = false;
    for index in 0..300 {
        app.append_entry(sample(index, "live message"));
    }
    let pointer_frame = |app: &mut CiallogcatApp, events: Vec<egui::Event>| {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 560.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                app.toolbar(ui);
                app.status_bar(ui);
                app.log_table(ui);
            },
        );
        output.textures_delta.clear();
    };
    pointer_frame(&mut app, vec![]);
    pointer_frame(&mut app, vec![]);
    assert!(app.follow_logs);
    let pos = egui::pos2(500.0, 350.0);
    pointer_frame(
        &mut app,
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    assert!(
        !app.follow_logs,
        "stop on press, before the click completes"
    );
    for index in 300..400 {
        app.append_entry(sample(index, "arrived while mouse held"));
    }
    pointer_frame(&mut app, vec![]);
    pointer_frame(
        &mut app,
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    let selected = app
        .selected
        .expect("release must still select the original row");
    assert!(
        selected < 300,
        "new arrivals must not replace the row under the mouse"
    );
    let output = key_frame(&mut app, &ctx, egui::Key::C, egui::Modifiers::COMMAND);
    assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == &app.entries[selected].copy_text())));
    assert_eq!(app.entries.len(), 400, "capture continues while browsing");
    key_frame(&mut app, &ctx, egui::Key::End, egui::Modifiers::COMMAND);
    assert!(app.follow_logs);
    key_frame(&mut app, &ctx, egui::Key::Home, egui::Modifiers::NONE);
    assert!(!app.follow_logs);
}
