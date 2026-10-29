use super::*;
use ui_test_support::Harness;

fn settle(app: &mut CiallogcatApp) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.filter_pending {
        app.poll_filter();
        assert!(Instant::now() < deadline, "filter did not complete");
        std::thread::yield_now();
    }
}

fn settle_hits(h: &mut Harness) {
    h.frame(vec![]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while h.app.detail_search.pending {
        h.frame(vec![]);
        assert!(Instant::now() < deadline, "detail search did not complete");
        std::thread::yield_now();
    }
}

#[test]
fn pause_follow_only_freezes_view_while_capture_keeps_admitting_records() {
    let mut h = Harness::new(false);
    let id = h.app.begin_test_capture();
    h.frame(vec![]);
    h.click_rendered_label("暂停跟随");
    assert!(!h.app.follow_logs);
    assert!(matches!(
        h.app.capture_controller.status(),
        ControllerStatus::Running
    ));
    h.app
        .backend_tx
        .send(BackendEvent::Logcat {
            session_id: id,
            event: LogcatEvent::Entry(LogEntry::marker("still captured").into()),
        })
        .unwrap();
    h.app.poll_backend(&h.ctx);
    assert_eq!(h.app.entries.len(), 1);
    assert!(!h.app.follow_logs);
    h.frame(vec![]);
    h.click_rendered_label("↓ 回到最新");
    assert!(h.app.follow_logs);
    assert!(matches!(
        h.app.capture_controller.status(),
        ControllerStatus::Running
    ));
}

#[test]
fn crash_navigation_reveals_filtered_unknown_process_with_original_context() {
    let mut h = Harness::new(false);
    h.app.package = "com.only.known".into();
    h.app.append_entry(LogEntry::marker("before"));
    h.app.append_entry(LogEntry::new(
        "",
        7,
        7,
        "AndroidRuntime",
        Level::Error,
        "FATAL EXCEPTION: main\njava.lang.IllegalStateException",
    ));
    h.app.append_entry(LogEntry::marker("after"));
    h.app.schedule_filter();
    settle(&mut h.app);
    assert!(!h.app.matches.contains(&1));
    let original = h.app.entries[1].copy_text();
    h.frame(vec![]);
    h.key(egui::Key::F8, egui::Modifiers::NONE);
    h.frame(vec![]);
    assert_eq!(h.app.selected, Some(1));
    assert_eq!(h.app.context_view.as_ref().unwrap().rows, [0, 1, 2]);
    assert_eq!(h.app.package, "com.only.known");
    assert_eq!(h.app.entries[1].copy_text(), original);
    assert!(h.labels.iter().any(|(label, _)| label == "Java 崩溃线索"));
}

#[test]
fn watched_application_follows_new_pids_without_rewriting_old_attribution() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    let id = app.begin_test_capture();
    app.package = "com.app".into();
    app.update_processes(HashMap::from([
        (10, "com.app".into()),
        (11, "com.app:worker".into()),
    ]));
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: id,
            event: LogcatEvent::Entry(
                LogEntry::new("", 10, 10, "App", Level::Info, "old process").into(),
            ),
        })
        .unwrap();
    app.poll_backend(&ctx);
    app.update_processes(HashMap::from([
        (20, "com.app".into()),
        (21, "com.app:worker".into()),
    ]));
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: id,
            event: LogcatEvent::Entry(
                LogEntry::new("", 21, 21, "App", Level::Info, "new worker").into(),
            ),
        })
        .unwrap();
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: id,
            event: LogcatEvent::Entry(
                LogEntry::new("", 10, 10, "App", Level::Info, "unknown old PID").into(),
            ),
        })
        .unwrap();
    app.poll_backend(&ctx);
    assert_eq!(app.entries[0].process.as_deref(), Some("com.app"));
    assert_eq!(app.entries[1].process.as_deref(), Some("com.app:worker"));
    assert!(app.entries[2].process.is_none());
    assert!(app.entries[0].matches_package("com.app") && app.entries[1].matches_package("com.app"));
    assert!(
        app.process_watch
            .recent
            .iter()
            .any(|change| change.pid == 10 && change.kind == "no_longer_observed")
    );
    assert!(
        app.process_watch
            .recent
            .iter()
            .any(|change| change.pid == 21 && change.kind == "appeared")
    );
    let facts = app.capture_facts.snapshot("running");
    assert!(
        facts.extra["recent_observations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "process_snapshot_change")
    );
    assert!(facts.known_gaps.is_empty());
}

#[test]
fn two_toolbar_rows_keep_tag_level_capture_and_file_controls_inside_small_window() {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    for label in [
        "设备",
        "↻ 刷新",
        "开始采集",
        "断开",
        "采集设置",
        "文件",
        "视图",
        "筛选方案",
        "清空视图",
        "暂停跟随",
        ".*",
        "Aa",
        "Tag",
        "V",
        "D",
        "I",
        "W",
        "E",
        "F",
    ] {
        let (_, rect) = h
            .label_bounds
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("{label} absent"));
        assert!(
            rect.min.x >= 0.0 && rect.max.x <= 900.0 && rect.min.y >= 0.0 && rect.max.y <= 90.0,
            "{label}: {rect:?}"
        );
    }
    let package = h
        .ctx
        .read_response(egui::Id::new("package_filter"))
        .unwrap()
        .rect;
    let query = h
        .ctx
        .read_response(egui::Id::new("query_shortcut"))
        .unwrap()
        .rect;
    assert!(query.width() >= 180.0 && package.width() >= 130.0);
    assert!((package.center().y - query.center().y).abs() < 1.0);
    let labels = [
        "设备",
        "↻ 刷新",
        "开始采集",
        "断开",
        "采集设置",
        "文件",
        "视图",
        "筛选方案",
        "清空视图",
        "暂停跟随",
    ];
    for (index, left) in labels.iter().enumerate() {
        let a = h
            .label_bounds
            .iter()
            .find(|(text, _)| text == left)
            .unwrap()
            .1;
        for right in &labels[index + 1..] {
            let b = h
                .label_bounds
                .iter()
                .find(|(text, _)| text == right)
                .unwrap()
                .1;
            assert!(!a.intersects(b), "toolbar labels overlap: {left}, {right}");
        }
    }
    h.click_label("Tag");
    h.click_input("tag_include");
    h.frame(vec![egui::Event::Text("网络".into())]);
    h.click_input("tag_exclude");
    h.frame(vec![egui::Event::Text("Noise".into())]);
    h.key(egui::Key::Escape, egui::Modifiers::NONE);
    h.app.schedule_filter();
    settle(&mut h.app);
    assert_eq!(h.app.active_filter.tag_include, "网络");
    assert_eq!(h.app.active_filter.tag_exclude, "Noise");
    assert!(h.app.app_config().tag_include == "网络");
    h.app.save_filter_name = "tag preset".into();
    h.app.save_current_filter();
    assert_eq!(h.app.saved_filters[0].tag_exclude, "Noise");
}

#[test]
fn context_reads_hidden_neighbors_and_returns_to_stable_origin_after_trim() {
    let mut h = Harness::new(false);
    h.app.follow_logs = false;
    for index in 0..140 {
        h.app.append_entry(LogEntry::new(
            "",
            7,
            7,
            "Tag",
            Level::Info,
            if index % 10 == 0 {
                format!("needle {index}")
            } else {
                format!("neighbor {index}")
            },
        ));
    }
    h.app.query = "needle".into();
    h.app.schedule_filter();
    settle(&mut h.app);
    h.app.selected = Some(60);
    h.app.selection.select(&h.app.matches, 60, false, false);
    h.frame(vec![]);
    let before = h.app.active_filter.clone();
    h.click_label("查看上下文");
    assert_eq!(h.app.reading_rows().len(), 101);
    assert_eq!(h.app.active_filter.query, before.query);
    h.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
    assert_eq!(
        h.app.selected,
        Some(61),
        "keyboard traverses unfiltered neighbors"
    );
    h.frame(vec![egui::Event::Copy]);
    assert!(h.copied[0].contains("neighbor 61"));
    Arc::make_mut(&mut h.app.entries).drain_front(20);
    h.app.rebase_after_trim(20);
    h.frame(vec![]);
    h.click_label("返回筛选位置");
    assert!(h.app.context_view.is_none());
    assert_eq!(h.app.selected, Some(40));
    assert_eq!(h.app.entries[40].message.as_ref(), "needle 60");
    h.click_label("查看上下文");
    Arc::make_mut(&mut h.app.entries).drain_front(50);
    h.app.rebase_after_trim(50);
    h.frame(vec![]);
    h.click_label("返回筛选位置");
    assert!(h.app.selected.is_none());
    assert!(h.app.copy_notice.as_ref().unwrap().contains("已过期"));
}

#[test]
fn context_restores_viewport_reference_and_filter_edits_exit_context() {
    let mut h = Harness::new(false);
    for index in 0..200 {
        h.app.append_entry(LogEntry::marker(if index % 2 == 0 {
            format!("needle {index}")
        } else {
            format!("neighbor {index}")
        }));
    }
    h.app.query = "needle".into();
    h.app.schedule_filter();
    settle(&mut h.app);
    h.frame(vec![]);
    h.app.table_scroll_offset = 12.25 * h.app.table_row_stride;
    h.app.show_context(80);
    Arc::make_mut(&mut h.app.entries).drain_front(10);
    h.app.rebase_after_trim(10);
    h.app.leave_context();
    assert_eq!(h.app.selected, Some(70));
    assert_eq!(
        h.app.restore_scroll_offset,
        Some(7.25 * h.app.table_row_stride),
        "return keeps the former top row and its fractional scroll position"
    );
    h.app.show_context(70);
    h.app.tag_exclude = "noise".into();
    h.app.queue_filter();
    assert!(h.app.context_view.is_none());
    assert_eq!(h.app.selected, Some(70));
}

#[test]
fn detail_navigation_finds_hidden_and_cross_segment_matches_without_full_shaping() {
    let mut h = Harness::new(false);
    let message = format!("{}needle{}NEEDLE", "中".repeat(7000), "尾".repeat(500));
    h.app
        .append_entry(LogEntry::new("", 7, 7, "Tag", Level::Info, message));
    h.app.query = "needle".into();
    h.app.schedule_filter();
    settle(&mut h.app);
    h.app.selected = Some(0);
    h.app.selection.select(&h.app.matches, 0, false, false);
    settle_hits(&mut h);
    assert_eq!(h.app.detail_search.hits.len(), 2);
    assert!(h.app.detail_search.hits[0].start > 2048);
    h.click_label("下一个");
    assert_eq!(h.app.detail_search.current, 0);
    assert!(h.labels.iter().any(|(text, _)| text.contains("needle")));
    h.click_label("下一个");
    assert_eq!(h.app.detail_search.current, 1);
    h.click_label("上一个");
    assert_eq!(h.app.detail_search.current, 0);
    h.key(egui::Key::F3, egui::Modifiers::NONE);
    assert_eq!(h.app.detail_search.current, 1);
    h.key(egui::Key::F3, egui::Modifiers::SHIFT);
    assert_eq!(h.app.detail_search.current, 0);
    assert!(
        h.labels.iter().map(|(text, _)| text.len()).max().unwrap() <= 2200,
        "only bounded segments and previews are shaped"
    );

    h.app.clear_logs();
    h.app.append_entry(LogEntry::new(
        "",
        7,
        7,
        "Tag",
        Level::Info,
        format!("{}abC{}", "x".repeat(511), "z".repeat(20000)),
    ));
    h.app.query = "(?i)abC".into();
    h.app.use_regex = true;
    h.app.schedule_filter();
    settle(&mut h.app);
    h.app.selected = Some(0);
    settle_hits(&mut h);
    assert_eq!(h.app.detail_search.hits.len(), 1);
    assert_eq!(h.app.detail_search.hits[0], 511..514);
    h.click_label("下一个");
    assert_eq!(h.app.detail_search.current, 0);
    h.app.query = "^abC".into();
    h.app.schedule_filter();
    settle(&mut h.app);
    assert!(
        h.app.matches.is_empty(),
        "anchored expression tests full message"
    );
}

#[test]
fn detail_match_cap_is_explicit_and_obsolete_results_cannot_replace_new_selection() {
    let mut h = Harness::new(false);
    h.app.append_entry(LogEntry::marker("a".repeat(10000)));
    h.app.append_entry(LogEntry::marker("a unique"));
    h.app.query = "a".into();
    h.app.schedule_filter();
    settle(&mut h.app);
    h.app.selected = Some(0);
    settle_hits(&mut h);
    assert_eq!(h.app.detail_search.hits.len(), reading::MAX_DETAIL_HITS);
    assert!(h.app.detail_search.capped);
    h.app.selected = Some(1);
    settle_hits(&mut h);
    assert_eq!(h.app.detail_search.hits.len(), 1);
    assert_eq!(h.app.detail_search.hits[0], 0..1);
    assert!(!h.app.detail_search.capped);
}

#[test]
fn context_anchor_stays_fully_visible_after_notice_reduces_table_height() {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    h.app.files.notice = Some("文件 · input.jsonl · 90 条".into());
    for index in 1..=90 {
        h.app.append_entry(LogEntry::new("10-23 12:00:00.000", 42, 43, "Network", Level::Error,
            if index % 7 == 0 { format!("#{index:03} timeout 请求超时\n    at example.Network\n    at example.App\ncaused by timeout") }
            else { format!("#{index:03} ordinary") }));
    }
    h.app.query = "timeout".into();
    h.app.schedule_filter();
    settle(&mut h.app);
    assert_eq!(h.app.matches.len(), 12);
    h.app.selected = Some(6);
    h.app.selection.select(&h.app.matches, 6, false, false);
    h.app.follow_logs = false;
    h.app.scroll_to_selected = true;
    h.frame(vec![]);
    h.frame(vec![]);
    h.click_label("查看上下文");
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(h.app.reading_rows().len(), 57);
    assert_eq!(h.app.selected, Some(6));
    let anchor = h
        .ctx
        .read_response(egui::Id::new(("log_row", h.app.log_epoch, 6usize)))
        .unwrap()
        .rect;
    let header_bottom = h
        .label_bounds
        .iter()
        .find(|(text, _)| text == "MESSAGE")
        .unwrap()
        .1
        .max
        .y;
    let details_top = h
        .label_bounds
        .iter()
        .find(|(text, _)| text == "日志详情")
        .unwrap()
        .1
        .min
        .y;
    assert!(
        anchor.min.y >= header_bottom && anchor.max.y < details_top - 3.0,
        "anchor {anchor:?} must be between header {header_bottom} and details {details_top}"
    );
}

#[test]
fn detail_identity_survives_native_reopen_and_cancels_prepared_logs() {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut entry = LogEntry::marker("x".repeat(100_000));
    let identity = session.next_record(Some(123456)).unwrap();
    entry.provenance = Some(identity);
    let original_copy = entry.copy_text();
    h.app.append_entry(entry);
    h.app.selected = Some(0);
    h.app.selection.select(&h.app.matches, 0, false, false);
    h.frame(vec![egui::Event::Copy]);
    assert!(h.app.copy_pending.is_some());
    let label = "复制引用";
    let rect = h
        .label_bounds
        .iter()
        .find(|(text, _)| text == label)
        .unwrap()
        .1;
    assert!(rect.min.x >= 0.0 && rect.max.x <= 900.0);
    h.pointer(rect.center(), true);
    h.pointer(rect.center(), false);
    assert_eq!(h.copied.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&h.copied[0]).unwrap(),
        serde_json::json!({"identity_scope":"capture_session","session_id":identity.session_id,"seq":1})
    );
    assert!(h.app.copy_pending.is_none());
    assert_eq!(h.app.entries[0].copy_text(), original_copy);
    let (_, tooltip, _) = h.app.evidence_reference(0).unwrap();
    assert!(
        tooltip.contains(&identity.session_id.to_string())
            && tooltip.contains("attempt: 1")
            && tooltip.contains("123456")
    );

    let native = ciallogcat::document::Document::from_snapshot(
        ciallogcat::document::DocumentHeader::live(
            session.id(),
            "fixture".into(),
            vec![LogBuffer::Main],
        ),
        Arc::clone(&h.app.entries),
        ciallogcat::document::ExportScope::default(),
        ciallogcat::document::CaptureStatus::default(),
    )
    .unwrap();
    let mut bytes = Vec::new();
    ciallogcat::document::write_snapshot(
        &mut bytes,
        &native,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    let document = ciallogcat::document::load(
        std::io::Cursor::new(bytes),
        16 * MIB,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    h.app.clear_logs();
    h.app.entries = Arc::clone(&document.entries);
    h.app.files.document = Some(Arc::new(document));
    h.app.schedule_filter();
    settle(&mut h.app);
    h.app.selected = Some(0);
    h.frame(vec![]);
    let label = "复制引用".to_owned();
    let pos = h.labels.iter().find(|(text, _)| text == &label).unwrap().1;
    h.pointer(pos, true);
    h.pointer(pos, false);
    assert_eq!(h.copied.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&h.copied[0]).unwrap(),
        serde_json::json!({"identity_scope":"capture_session","session_id":identity.session_id,"seq":identity.seq})
    );
    h.app.files.document = None;
    h.app.clear_logs();
    h.app.append_entry(LogEntry::marker("unowned"));
    assert!(h.app.evidence_reference(0).is_none());
}

#[test]
fn history_scope_is_selectable_from_capture_menu_and_applies_after_submenu() {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    h.click_rendered_label("采集设置");
    h.click_rendered_label("最近 N 条后跟随");
    h.click_rendered_label("最近 N 条快照");
    assert!(h.app.scope_draft.parse().unwrap().is_dump());
    h.click_rendered_label("应用");
    assert!(h.app.capture_scope.is_dump());
    h.click_rendered_label("采集设置");
    h.click_rendered_label("最近 N 条快照");
    h.click_rendered_label("当前 buffer 快照");
    h.click_rendered_label("应用");
    assert_eq!(h.app.capture_scope.logcat_args(), vec!["-d"]);
}

#[test]
fn diagnostics_menu_renders_without_a_file_modal() {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    h.click_rendered_label("采集设置");
    h.click_rendered_label("只读设备诊断…");
    h.frame(vec![]);
    assert!(!h.app.files.is_modal());
    assert!(h.labels.iter().any(|(label, _)| label == "只读设备诊断"));
    assert!(
        h.labels
            .iter()
            .any(|(label, _)| label.contains("请选择设备并配置 ADB"))
    );
}

#[test]
fn native_structural_rows_share_kind_aware_table_details_accessibility_and_copy() {
    use ciallogcat::document;
    use egui::accesskit::Role;

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.follow_logs = false;
    app.show_details = false;
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let messages = ["boundary 雪\nsecond line\n", "raw\0 bytes  "];
    for mut entry in [
        LogEntry::marker(messages[0]),
        LogEntry::unparsed(messages[1]),
        LogEntry::new("device-time", 0, 0, "logcat", Level::Info, "real zero IDs"),
    ] {
        entry.provenance = Some(session.next_record(Some(123456)).unwrap());
        app.append_entry(entry);
    }
    let native = document::Document::from_snapshot(
        document::DocumentHeader::live(session.id(), "fixture".into(), vec![LogBuffer::Main]),
        Arc::clone(&app.entries),
        document::ExportScope::default(),
        document::CaptureStatus::default(),
    )
    .unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut bytes = Vec::new();
    document::write_snapshot(&mut bytes, &native, &cancel).unwrap();
    let document = document::load(std::io::Cursor::new(bytes), 16 * MIB, &cancel).unwrap();
    app.entries = Arc::clone(&document.entries);
    app.files.document = Some(Arc::new(document));
    app.schedule_filter();
    settle(&mut app);
    assert_eq!(app.matches, [0, 1, 2]);
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    for (index, kind) in ["event", "unparsed"].into_iter().enumerate() {
        let row = &h
            .accessible_nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Row && node.row_index() == Some(index + 1))
            .unwrap()
            .1;
        let summary = row.label().unwrap();
        assert!(summary.starts_with(&format!("{kind} row {}. Message ", index + 1)));
        for fake in ["Time ", "Level ", "PID / TID", "Process ", "Tag ", "logcat"] {
            assert!(!summary.contains(fake), "{summary}");
        }
        assert_eq!(row.children().len(), 6);
        for (column, child) in row.children().iter().enumerate() {
            let cell = &h
                .accessible_nodes
                .iter()
                .find(|(id, _)| id == child)
                .unwrap()
                .1;
            if column < 5 {
                assert_eq!(cell.label().unwrap_or_default(), "");
            } else {
                assert_eq!(
                    cell.label().unwrap(),
                    format!("[{kind}] {}", preview_first_line(messages[index]))
                );
            }
        }
    }
    let log = &h
        .accessible_nodes
        .iter()
        .find(|(_, node)| node.role() == Role::Row && node.row_index() == Some(3))
        .unwrap()
        .1;
    assert!(log.label().unwrap().contains("Level Info. PID / TID 0 / 0"));
    assert!(log.label().unwrap().contains("Tag logcat"));
    for (index, kind) in ["event", "unparsed"].into_iter().enumerate() {
        h.app.show_details = true;
        h.app.selected = Some(index);
        h.app.selection.select(&h.app.matches, index, false, false);
        h.frame(vec![]);
        assert!(
            h.labels
                .iter()
                .any(|(label, _)| label == &format!("[{kind}] · 非日志记录；无日志元字段"))
        );
        h.frame(vec![egui::Event::Copy]);
        assert_eq!(h.copied, [format!("[{kind}] {}", messages[index])]);
        assert_eq!(h.app.entries[index].message.as_ref(), messages[index]);
    }
}

#[test]
fn gui_filter_worker_does_not_treat_structural_placeholders_as_metadata() {
    let mut h = Harness::new(false);
    h.app.append_entry(LogEntry::marker("matching body"));
    h.app.append_entry(LogEntry::unparsed("matching body"));
    let mut log = LogEntry::new("", 0, 0, "logcat", Level::Info, "matching body");
    log.process = Some("com.example.app".into());
    h.app.append_entry(log);
    for (package, tag_include, tag_exclude, min_level, expected) in [
        ("", "", "", Level::Verbose, vec![0, 1, 2]),
        ("", "", "", Level::Debug, vec![2]),
        ("", "logcat", "", Level::Verbose, vec![2]),
        ("", "", "Absent", Level::Verbose, vec![2]),
        ("com.example.app", "", "", Level::Verbose, vec![2]),
        ("", "", "", Level::Verbose, vec![0, 1, 2]),
    ] {
        h.app.package = package.into();
        h.app.tag_include = tag_include.into();
        h.app.tag_exclude = tag_exclude.into();
        h.app.min_level = min_level;
        h.app.query = "matching".into();
        h.app.schedule_filter();
        settle(&mut h.app);
        assert_eq!(h.app.matches, expected);
    }
}

#[test]
fn structural_kind_label_does_not_create_or_shift_message_highlights() {
    for pattern in ["event", "^raw"] {
        let matcher = Regex::new(pattern).unwrap();
        let job = highlighted_message("[event] ", "raw body", Some(&matcher), Color32::WHITE, true);
        assert_eq!(job.text, "[event] raw body");
        let hits: Vec<_> = job
            .sections
            .iter()
            .filter(|section| section.format.background != Color32::TRANSPARENT)
            .map(|section| &job.text[section.byte_range.start.0..section.byte_range.end.0])
            .collect();
        assert_eq!(
            hits,
            if pattern == "event" {
                vec![]
            } else {
                vec!["raw"]
            }
        );
    }
}

#[test]
fn tag_only_empty_results_name_the_active_filter_at_small_window_size() {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    h.app.append_entry(LogEntry::new(
        "",
        1,
        1,
        "Network",
        Level::Info,
        "still retained",
    ));
    for (include, exclude) in [("Other", ""), ("", "Network")] {
        h.app.tag_include = include.into();
        h.app.tag_exclude = exclude.into();
        h.app.schedule_filter();
        settle(&mut h.app);
        h.frame(vec![]);
        assert!(h.app.matches.is_empty());
        let (_, bounds) = h
            .label_bounds
            .iter()
            .find(|(label, _)| {
                label.starts_with("没有匹配日志。") && label.contains("Tag 包含/排除")
            })
            .expect("empty-result guidance must name both Tag conditions");
        assert!(bounds.min.x >= 0.0 && bounds.max.x <= 900.0);
        assert!(bounds.min.y >= 88.0 && bounds.max.y <= 560.0);
        assert_eq!(
            h.app.entries.len(),
            1,
            "guidance must not clear retained records"
        );
    }
}

#[test]
fn no_device_empty_guidance_exposes_the_working_offline_open_shortcut() {
    for state in [CaptureState::NoAdb, CaptureState::WaitingForDevice] {
        let mut h = Harness::new(false);
        h.resize(900.0, 560.0);
        h.app.capture_state = state;
        h.frame(vec![]);
        let (_, bounds) = h
            .label_bounds
            .iter()
            .find(|(label, _)| {
                label.contains("原生 JSONL")
                    && label.contains("文件 / Ctrl+O")
                    && label.contains("无需设备")
            })
            .expect("empty-state guidance must expose offline file reading");
        assert!(bounds.min.x >= 0.0 && bounds.max.x <= 900.0);
        assert!(bounds.min.y >= 88.0 && bounds.max.y <= 560.0);
        h.key(egui::Key::O, egui::Modifiers::COMMAND);
        assert!(
            h.app.files.is_modal(),
            "advertised shortcut opens the path dialog without ADB"
        );
        assert!(h.labels.iter().any(|(label, _)| label == "打开日志文件"));
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.files.is_modal());
        assert!(h.app.entries.is_empty());
    }
}
