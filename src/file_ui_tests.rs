use super::*;
use std::fs;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ciallogcat-file-ui-{}",
            ciallogcat::session::SessionId::new()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn native(seq: u64, message: &str) -> Vec<u8> {
    let mut session = ciallogcat::session::Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut entry = LogEntry::new("10-23 00:00:00.000", 1, 2, "tag", Level::Info, message);
    let mut identity = session.next_record(Some(1234)).unwrap();
    identity.seq = seq;
    entry.provenance = Some(identity);
    let header =
        document::DocumentHeader::live(session.id(), "fixture".into(), vec![LogBuffer::Main]);
    let mut encoder = document::StreamEncoder::new(header).unwrap();
    [
        encoder.header_bytes().unwrap(),
        encoder.record_bytes(&entry).unwrap(),
        encoder
            .finish_bytes(document::CaptureStatus::default())
            .unwrap(),
    ]
    .concat()
}

fn wait_job(app: &mut CiallogcatApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.files.job.is_some() {
        assert!(Instant::now() < deadline, "file worker did not finish");
        app.poll_files(ctx);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn live_entry(app: &mut CiallogcatApp, ctx: &egui::Context, message: &str) {
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: app.capture_session_id,
            event: LogcatEvent::Entry(
                LogEntry::new("10-23 00:00:00.000", 1, 2, "tag", Level::Info, message).into(),
            ),
        })
        .unwrap();
    app.poll_backend(ctx);
}

#[test]
fn native_gui_open_filter_and_save_keep_original_payload_and_gaps() {
    let directory = Directory::new();
    let input = directory.0.join("native.jsonl");
    let filtered = directory.0.join("filtered.jsonl");
    let retained = directory.0.join("retained.jsonl");
    let payload = vec![
        0x08, 1, 0x18, 4, 0x28, 7, 0x30, 7, 0x3a, 1, b'T', 0x42, 8, 0xe2, 0x82, 0xbf, b'\n', b'b',
        b'o', b'd', b'y',
    ];
    let mut entry = ciallogcat::protobuf::DecodedEntry::decode_payload(payload.clone())
        .unwrap()
        .project();
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    entry.provenance = session.next_record(Some(42));
    let identity = entry.provenance;
    let mut header = DocumentHeader::live(session.id(), "fixture".into(), LogBuffer::defaults());
    header.source.transport_format = Some("logcat_proto_experimental".into());
    let mut entries = LogStore::default();
    entries.push(entry);
    let capture = CaptureStatus {
        completeness: "partial".into(),
        known_gaps: vec![
            serde_json::json!({"kind":"stream_interruptions","count":1,"loss":"unknown"}),
        ],
        ..CaptureStatus::default()
    };
    let doc = Document::from_snapshot(header, Arc::new(entries), ExportScope::default(), capture)
        .unwrap();
    document::write_snapshot(File::create(&input).unwrap(), &doc, &AtomicBool::new(false)).unwrap();
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.start_file_job(Action::Open, input, &ctx);
    wait_job(&mut app, &ctx);
    assert_eq!(app.entries[0].message.as_ref(), "₿\nbody");
    assert_eq!(app.entries[0].provenance, identity);
    assert_eq!(
        app.files.document.as_ref().unwrap().header.schema_version,
        3
    );
    app.query = "body".into();
    app.schedule_filter();
    app.start_file_job(Action::Export, filtered.clone(), &ctx);
    wait_job(&mut app, &ctx);
    app.start_file_job(Action::Save, retained.clone(), &ctx);
    wait_job(&mut app, &ctx);
    for path in [filtered, retained] {
        let saved = document::load(
            BufReader::new(File::open(path).unwrap()),
            16 * MIB,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(saved.header.schema_version, 3);
        assert_eq!(
            saved.entries[0].proto.as_ref().unwrap().raw_payload,
            payload
        );
        assert_eq!(saved.entries[0].provenance, identity);
        assert_eq!(saved.end.capture.known_gaps, doc.end.capture.known_gaps);
    }
    assert_eq!(
        app.files.document.as_ref().unwrap().header.schema_version,
        3
    );
}

fn snapshot_bytes(document: &Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document::write_snapshot(&mut bytes, document, &AtomicBool::new(false)).unwrap();
    bytes
}

fn jsonl_rows(bytes: &[u8]) -> Vec<serde_json::Value> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

fn offline_filter_fixture() -> Document {
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut entries = LogStore::default();
    for message in ["unrelated", "checkout AUTH_EXPIRED", "checkout approved"] {
        let mut payload = vec![0x08, 1, 0x18, 4, 0x28, 7, 0x30, 7, 0x3a, 1, b'T', 0x42];
        payload.push(u8::try_from(message.len()).unwrap());
        payload.extend_from_slice(message.as_bytes());
        let mut entry = ciallogcat::protobuf::DecodedEntry::decode_payload(payload)
            .unwrap()
            .project();
        entry.provenance = session.next_record(Some(42));
        entries.push(entry);
    }
    let mut header = DocumentHeader::live(session.id(), "fixture".into(), vec![LogBuffer::Crash]);
    header.source.transport_format = Some("logcat_proto_experimental".into());
    header.extra.insert(
        "view_filter".into(),
        serde_json::json!({"package":"com.example.shop","query":"old view","minimum_level":"V",
            "tag_include":"old tag","tag_exclude":"","regex":false,"case_sensitive":false}),
    );
    header.source.extra.insert(
        "capture_scope".into(),
        serde_json::json!({"mode":"follow","start":{"kind":"tail","value":1}}),
    );
    let capture = CaptureStatus {
        state: "paused".into(),
        completeness: "partial".into(),
        known_gaps: vec![
            serde_json::json!({"kind":"stream_interruptions","count":1,"loss":"unknown"}),
        ],
        ..CaptureStatus::default()
    };
    let mut document =
        Document::from_snapshot(header, Arc::new(entries), ExportScope::default(), capture)
            .unwrap();
    for extra in [
        &mut document.header.extra,
        &mut document.header.source.extra,
        &mut document.header.normalization,
        &mut document.end.extra,
        &mut document.end.scope.extra,
        &mut document.end.capture.extra,
    ] {
        extra.insert(
            "future".into(),
            serde_json::json!({"nested":[1,"preserved"]}),
        );
    }
    for metadata in &mut document.record_metadata {
        metadata
            .extra
            .insert("future_row".into(), serde_json::json!({"seq":metadata.seq}));
        metadata
            .proto_extra
            .insert("future_proto".into(), serde_json::json!(["preserved"]));
    }
    document
}

fn offline_exports_freeze_view_filter() {
    let directory = Directory::new();
    let mut input = directory.0.join("source.jsonl");
    fs::write(&input, snapshot_bytes(&offline_filter_fixture())).unwrap();
    // Reopen the first export in a fresh app before filtering it a second time.
    for (round, (query, selected_rows)) in [("checkout", vec![2, 3]), ("AUTH_EXPIRED", vec![1])]
        .into_iter()
        .enumerate()
    {
        let original_bytes = fs::read(&input).unwrap();
        let original = jsonl_rows(&original_bytes);
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.start_file_job(Action::Open, input.clone(), &ctx);
        wait_job(&mut app, &ctx);
        let opened = Arc::clone(app.files.document.as_ref().expect("file opened"));
        assert_eq!(snapshot_bytes(&opened), original_bytes);
        app.query = query.into();
        app.tag_include = "T".into();
        app.tag_exclude = "Noise".into();
        app.min_level = Level::Info;
        app.use_regex = true;
        app.case_sensitive = true;
        app.schedule_filter();
        let expected_view = serde_json::json!({"package":"","query":query,"minimum_level":"I",
            "tag_include":"T","tag_exclude":"Noise","regex":true,"case_sensitive":true});
        let output = directory.0.join(format!("filtered-{round}.jsonl"));
        app.start_file_job(Action::Export, output.clone(), &ctx);
        app.query = "changed after submitting export".into();
        app.tag_include.clear();
        app.use_regex = false;
        app.schedule_filter();
        wait_job(&mut app, &ctx);
        let exported = document::load(
            BufReader::new(File::open(&output).unwrap()),
            16 * MIB,
            &AtomicBool::new(false),
        )
        .unwrap();
        let rows = jsonl_rows(&fs::read(&output).unwrap());
        assert_eq!(exported.entries.len(), selected_rows.len());
        for (row, &source_row) in rows[1..rows.len() - 1].iter().zip(&selected_rows) {
            // Includes original session/seq, provenance, payload and row extensions.
            assert_eq!(row, &original[source_row]);
        }
        assert_eq!(rows[0]["view_filter"], expected_view);
        assert_ne!(rows[0]["artifact_id"], original[0]["artifact_id"]);
        let mut expected_header = original[0].clone();
        expected_header["artifact_id"] = rows[0]["artifact_id"].clone();
        expected_header["derived_from_artifact"] = original[0]["artifact_id"].clone();
        expected_header["view_filter"] = expected_view.clone();
        assert_eq!(rows[0], expected_header);
        let mut expected_end = original.last().unwrap().clone();
        expected_end["artifact_id"] = rows[0]["artifact_id"].clone();
        expected_end["record_count"] = selected_rows.len().into();
        expected_end["first_seq"] = original[selected_rows[0]]["seq"].clone();
        expected_end["last_seq"] = original[*selected_rows.last().unwrap()]["seq"].clone();
        expected_end["scope"]["kind"] = "filtered".into();
        expected_end["scope"]["filter"] = if round == 0 {
            expected_view
        } else {
            serde_json::json!({"all_of":[original.last().unwrap()["scope"]["filter"],expected_view]})
        };
        // Original source range, nonempty gaps, capture facts and extensions survive.
        assert_eq!(rows.last().unwrap(), &expected_end);

        {
            let name = "retained";
            let action = Action::Save;
            let saved_path = directory.0.join(format!("{name}-{round}.jsonl"));
            app.start_file_job(action, saved_path.clone(), &ctx);
            wait_job(&mut app, &ctx);
            let saved = document::load(
                BufReader::new(File::open(&saved_path).unwrap()),
                16 * MIB,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(saved.entries.len(), opened.entries.len());
            assert_eq!(
                saved.header.extra["view_filter"],
                original[0]["view_filter"]
            );
            assert_eq!(
                serde_json::to_value(&saved.header.source).unwrap(),
                original[0]["source"]
            );
            assert_eq!(
                serde_json::to_value(&saved.end.scope).unwrap(),
                original.last().unwrap()["scope"]
            );
            assert_eq!(
                serde_json::to_value(&saved.end.capture).unwrap(),
                original.last().unwrap()["capture"]
            );
            assert_eq!(fs::read(saved_path).unwrap(), original_bytes);
        }
        assert!(Arc::ptr_eq(app.files.document.as_ref().unwrap(), &opened));
        assert!(Arc::ptr_eq(&app.entries, &opened.entries));
        assert_eq!(snapshot_bytes(&opened), original_bytes);
        assert_eq!(fs::read(&input).unwrap(), original_bytes);
        input = output;
    }
}

#[test]
fn offline_gui_export_freezes_view_filter() {
    offline_exports_freeze_view_filter();
}

#[test]
fn diagnostic_context_is_only_exported_for_its_observed_device() {
    let directory = Directory::new();
    let report = ciallogcat::diagnostics::collect(
        &directory.0.join("nonexistent-adb"),
        "fixture",
        &LogBuffer::defaults(),
        &ciallogcat::diagnostics::DiagnosticOptions::default(),
        &ciallogcat::diagnostics::CancellationToken::default(),
    )
    .unwrap();
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.begin_test_capture();
    app.diagnostics.report = Some(Arc::new(report));
    let document = app
        .snapshot_for_save()
        .unwrap()
        .document(&AtomicBool::new(false))
        .unwrap();
    assert_eq!(
        document.header.source.extra["device_diagnostics"]["serial"],
        "fixture"
    );
    assert_eq!(
        document.header.source.extra["device_diagnostics"]["complete"],
        false
    );
    app.retained_device = Some("other-device".into());
    assert!(app.diagnostic_context().is_none());
}

#[test]
fn observed_interruptions_survive_save_reopen_and_filtered_export() {
    let directory = Directory::new();
    let path = directory.0.join("interrupted.jsonl");
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    let first_attempt = app.begin_test_capture();
    live_entry(&mut app, &ctx, "before disconnect");
    app.backend_tx
        .send(BackendEvent::Devices(Err("discovery failed".into())))
        .unwrap();
    app.poll_backend(&ctx);
    let before = app
        .snapshot_for_save()
        .unwrap()
        .document(&AtomicBool::new(false))
        .unwrap();
    assert_eq!(before.end.capture.completeness, "unknown");
    assert!(before.end.capture.known_gaps.is_empty());
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: first_attempt,
            event: LogcatEvent::Exited {
                code: Some(17),
                error: Some("USB disconnected".into()),
                incomplete_proto_frame: false,
            },
        })
        .unwrap();
    app.poll_backend(&ctx);
    app.buffers = vec![LogBuffer::Crash];
    app.begin_test_capture();
    live_entry(&mut app, &ctx, "after reconnect");
    app.start_file_job(Action::Save, path.clone(), &ctx);
    wait_job(&mut app, &ctx);
    let saved = document::load(
        BufReader::new(File::open(path).unwrap()),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(saved.end.capture.completeness, "partial");
    assert_eq!(
        saved.end.capture.known_gaps[0]["kind"],
        "stream_interruptions"
    );
    assert_eq!(saved.end.capture.known_gaps[0]["count"], 1);
    assert_eq!(saved.entries[0].provenance.unwrap().segment, 1);
    assert_eq!(saved.entries[1].provenance.unwrap().segment, 2);
    let observations = saved.end.capture.extra["recent_observations"]
        .as_array()
        .unwrap();
    let starts: Vec<_> = observations
        .iter()
        .filter(|o| o["kind"] == "attempt_started")
        .collect();
    assert_ne!(
        starts[0]["details"]["requested_buffers"],
        starts[1]["details"]["requested_buffers"]
    );
    let selected = saved
        .select(&[1], serde_json::json!({"query":"after"}))
        .unwrap();
    let mut bytes = Vec::new();
    document::write_snapshot(&mut bytes, &selected, &AtomicBool::new(false)).unwrap();
    let reopened =
        document::load(io::Cursor::new(bytes), 16 * MIB, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        reopened.end.capture.known_gaps,
        saved.end.capture.known_gaps
    );
    assert_eq!(reopened.end.capture.extra, saved.end.capture.extra);
    assert_eq!(reopened.entries[0].provenance, saved.entries[1].provenance);
}

#[test]
fn view_clear_preserves_session_facts_and_stale_attempt_cannot_taint_new_source() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    let attempt = app.begin_test_capture();
    let session = app.live_session.id();
    live_entry(&mut app, &ctx, "cleared locally");
    app.clear_logs();
    assert_eq!(app.live_session.id(), session);
    live_entry(&mut app, &ctx, "retained");
    app.pause_capture();
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: attempt,
            event: LogcatEvent::Exited {
                code: None,
                error: None,
                incomplete_proto_frame: false,
            },
        })
        .unwrap();
    app.poll_backend(&ctx);
    let saved = app
        .snapshot_for_save()
        .unwrap()
        .document(&AtomicBool::new(false))
        .unwrap();
    assert_eq!(saved.end.scope.omitted_before, Some(1));
    assert_eq!(saved.end.scope.extra["view_cleared_records"], 1);
    assert_eq!(saved.entries[0].provenance.unwrap().seq, 2);
    assert!(saved.end.capture.known_gaps.is_empty());
    app.switch_device("different-device".into(), &ctx);
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: attempt,
            event: LogcatEvent::Exited {
                code: Some(9),
                error: Some("stale failure".into()),
                incomplete_proto_frame: false,
            },
        })
        .unwrap();
    app.poll_backend(&ctx);
    let fresh = app
        .snapshot_for_save()
        .unwrap()
        .document(&AtomicBool::new(false))
        .unwrap();
    assert!(fresh.end.capture.known_gaps.is_empty());
    assert_eq!(fresh.end.capture.extra["attempts"], 0);
    assert_eq!(fresh.end.scope.extra["view_cleared_records"], 0);
}

#[test]
fn failed_and_cancelled_imports_preserve_current_source_and_view() {
    let directory = Directory::new();
    let path = directory.0.join("invalid.jsonl");
    fs::write(&path, b"{broken}\n").unwrap();
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.begin_test_capture();
    live_entry(&mut app, &ctx, "old log");
    app.selected = Some(0);
    let old = Arc::clone(&app.entries);
    let session = app.live_session.id();
    app.start_file_job(Action::Open, path.clone(), &ctx);
    wait_job(&mut app, &ctx);
    assert!(Arc::ptr_eq(&old, &app.entries));
    assert_eq!(app.selected, Some(0));
    assert_eq!(app.live_session.id(), session);
    assert!(matches!(
        app.capture_controller.intent(),
        CaptureIntent::Capture(_)
    ));
    fs::write(&path, native(7, "valid")).unwrap();
    app.start_file_job(Action::Open, path, &ctx);
    app.files
        .job
        .as_ref()
        .unwrap()
        .cancel
        .store(true, Ordering::Release);
    wait_job(&mut app, &ctx);
    assert!(app.files.candidate.is_none());
    assert!(Arc::ptr_eq(&old, &app.entries));
}

#[test]
fn native_import_is_read_only_and_preserves_capture_identity() {
    let directory = Directory::new();
    let path = directory.0.join("native.jsonl");
    fs::write(&path, native(7, "unattributed")).unwrap();
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.package = "com.previous.device".into();
    app.start_file_job(Action::Open, path, &ctx);
    wait_job(&mut app, &ctx);
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].provenance.unwrap().seq, 7);
    assert!(app.entries[0].process_source.is_none());
    let document = app.files.document.as_ref().unwrap();
    assert_eq!(
        document.header.session_id,
        Some(app.entries[0].provenance.unwrap().session_id)
    );
    assert!(app.package.is_empty());
    assert!(app.active_filter.package.is_empty());
    assert_eq!(document.record_metadata[0].seq, 7);
    let before = app.memory_limit_mib;
    app.files.document_bytes = 32 * MIB;
    app.apply_memory_budget(16);
    assert_eq!(app.memory_limit_mib, before);
    assert_eq!(app.entries.len(), 1);
}

#[test]
fn cancel_after_publication_keeps_saved_file_but_never_opens_candidate() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.append_entry(LogEntry::marker("old view"));
    let old = Arc::clone(&app.entries);
    let candidate = document::load(
        io::Cursor::new(native(1, "new view")),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    let bytes = candidate.memory_bytes();
    app.files.candidate = Some((candidate, PathBuf::from("source.jsonl"), bytes));
    let (sender, receiver) = mpsc::channel();
    sender
        .send(Ok(Completed::Saved(document::WriteSummary {
            records: 1,
            bytes: 1024,
        })))
        .unwrap();
    app.files.job = Some(Job {
        cancel: Arc::new(AtomicBool::new(true)),
        receiver,
        path: PathBuf::from("saved.jsonl"),
        action: Action::SaveBeforeOpen,
    });
    app.poll_files(&ctx);
    assert!(Arc::ptr_eq(&old, &app.entries));
    assert!(app.files.candidate.is_none());
    assert!(app.files.notice.as_ref().unwrap().contains("已保存"));
    assert!(app.files.notice.as_ref().unwrap().contains("已取消打开"));
}

#[test]
fn file_modal_pointer_cancel_preserves_background_follow_and_selection() {
    for following in [true, false] {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        for index in 0..40 {
            app.append_entry(LogEntry::marker(format!("row-{index}")));
        }
        app.follow_logs = following;
        app.selected = Some(2);
        app.selection.select(&app.matches, 2, false, false);
        app.show_details = false;
        let mut harness =
            crate::ui_test_support::Harness::from_app(ctx, app, egui::vec2(900.0, 560.0));
        let background_row = harness.app.visible_log_rows[0].0.accesskit_id();
        harness.key(egui::Key::O, egui::Modifiers::COMMAND);
        harness.frame(vec![]);
        harness.frame(vec![]);
        harness.frame(vec![egui::Event::AccessKitActionRequest(
            egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Focus,
                target_tree: egui::accesskit::TreeId::ROOT,
                target_node: background_row,
                data: None,
            },
        )]);
        assert_eq!(harness.app.follow_logs, following);
        let position = harness
            .labels
            .iter()
            .find(|(label, _)| label == "取消")
            .unwrap()
            .1;
        harness.pointer(position, true);
        assert_eq!(harness.app.follow_logs, following);
        harness.pointer(position, false);
        harness.frame(vec![]);
        assert!(harness.app.files.dialog.is_none());
        assert_eq!(harness.app.follow_logs, following);
        assert_eq!(harness.app.selected, Some(2));
        assert_eq!(harness.app.entries.len(), 40);
    }
}

#[test]
fn candidate_arrival_blocks_release_on_a_previously_pressed_capture_button() {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.begin_test_capture();
    live_entry(&mut app, &ctx, "old log");
    let mut harness = crate::ui_test_support::Harness::from_app(ctx, app, egui::vec2(900.0, 560.0));
    let pause = harness
        .labels
        .iter()
        .find(|(label, _)| label == "暂停采集")
        .unwrap()
        .1;
    harness.pointer(pause, true);
    let document = document::load(
        io::Cursor::new(native(7, "new file")),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    let bytes = document.memory_bytes();
    harness.app.files.candidate = Some((document, PathBuf::from("new.jsonl"), bytes));
    harness.pointer(pause, false);
    assert!(matches!(
        harness.app.capture_controller.intent(),
        CaptureIntent::Capture(_)
    ));
    harness.frame(vec![]);
    harness.click_rendered_label("取消");
    assert!(harness.app.files.candidate.is_none());
    assert!(matches!(
        harness.app.capture_controller.intent(),
        CaptureIntent::Capture(_)
    ));
    assert_eq!(harness.app.entries[0].message.as_ref(), "old log");
}

#[test]
fn filtered_save_freezes_rows_and_spec_while_capture_continues() {
    let directory = Directory::new();
    let path = directory.0.join("filtered.jsonl");
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.begin_test_capture();
    live_entry(&mut app, &ctx, "ignore first");
    live_entry(&mut app, &ctx, "keep second");
    app.active_filter.query = "keep".into();
    app.active_filter.tag_include = "ta".into();
    app.active_filter.tag_exclude = "Noise".into();
    app.start_file_job(Action::Export, path.clone(), &ctx);
    app.active_filter.query = "changed after save".into();
    app.active_filter.tag_include = "changed after save".into();
    live_entry(&mut app, &ctx, "keep later");
    wait_job(&mut app, &ctx);
    let doc = document::load(
        BufReader::new(File::open(path).unwrap()),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.entries.len(), 1);
    assert_eq!(doc.entries[0].message.as_ref(), "keep second");
    assert_eq!(doc.entries[0].provenance.unwrap().seq, 2);
    assert_eq!(doc.end.scope.source_last_seq, Some(2));
    assert_eq!(doc.end.scope.filter.as_ref().unwrap()["query"], "keep");
    assert_eq!(doc.end.scope.filter.as_ref().unwrap()["tag_include"], "ta");
    assert_eq!(
        doc.header.extra["view_filter"],
        *doc.end.scope.filter.as_ref().unwrap()
    );
    assert_eq!(
        doc.end.scope.filter.as_ref().unwrap()["tag_exclude"],
        "Noise"
    );
    assert_eq!(app.entries.len(), 3);
}

#[test]
fn save_before_open_failure_preserves_old_document_and_success_replaces_it() {
    let directory = Directory::new();
    let output = directory.0.join("saved.jsonl");
    fs::write(&output, b"do not overwrite").unwrap();
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.begin_test_capture();
    live_entry(&mut app, &ctx, "old evidence");
    let old = Arc::clone(&app.entries);
    let candidate = || {
        let doc = document::load(
            io::Cursor::new(native(42, "new file")),
            16 * MIB,
            &AtomicBool::new(false),
        )
        .unwrap();
        let bytes = doc.memory_bytes();
        (doc, directory.0.join("source.jsonl"), bytes)
    };
    app.files.candidate = Some(candidate());
    app.start_file_job(Action::SaveBeforeOpen, output.clone(), &ctx);
    wait_job(&mut app, &ctx);
    assert!(Arc::ptr_eq(&old, &app.entries));
    assert!(app.files.candidate.is_none());
    assert_eq!(fs::read(&output).unwrap(), b"do not overwrite");
    fs::remove_file(&output).unwrap();
    app.files.candidate = Some(candidate());
    app.close_capture();
    app.backend_tx
        .send(BackendEvent::Logcat {
            session_id: app.capture_session_id,
            event: LogcatEvent::Exited {
                code: Some(0),
                error: None,
                incomplete_proto_frame: false,
            },
        })
        .unwrap();
    app.poll_backend(&ctx);
    app.start_file_job(Action::SaveBeforeOpen, output.clone(), &ctx);
    wait_job(&mut app, &ctx);
    let saved = document::load(
        BufReader::new(File::open(output).unwrap()),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(saved.entries[0].message.as_ref(), "old evidence");
    assert_eq!(app.entries[0].message.as_ref(), "new file");
    assert!(app.files.document.is_some());
    assert!(matches!(
        app.capture_controller.intent(),
        CaptureIntent::Closed
    ));
}

#[test]
fn export_and_prepared_copy_share_frozen_history_and_candidate_modal_blocks_commit() {
    let directory = Directory::new();
    let path = directory.0.join("filtered.jsonl");
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.begin_test_capture();
    for index in 0..32 {
        live_entry(&mut app, &ctx, &format!("{index} {}", "x".repeat(4096)));
    }
    app.selection.select_all(&app.matches);
    app.selected = Some(0);
    let expected = app.selection.copy_text(&app.entries);
    app.copy_logs(&ctx, CopyTarget::Selection);
    app.start_file_job(Action::Export, path.clone(), &ctx);
    live_entry(&mut app, &ctx, "new tail excluded from both requests");
    wait_job(&mut app, &ctx);
    let exported = document::load(
        BufReader::new(File::open(path).unwrap()),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(exported.entries.len(), 32);
    let mut h = crate::ui_test_support::Harness::from_app(ctx, app, egui::vec2(900.0, 560.0));
    let deadline = Instant::now() + Duration::from_secs(3);
    while h.app.copy_ready.is_none() {
        h.frame(vec![]);
        assert!(h.copied.is_empty());
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    h.frame(vec![]);
    let ready = h
        .labels
        .iter()
        .find(|(label, _)| label == "复制已准备好")
        .unwrap()
        .1;
    h.pointer(ready, true);
    let candidate = document::load(
        io::Cursor::new(native(1, "candidate")),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    let bytes = candidate.memory_bytes();
    h.app.files.candidate = Some((candidate, directory.0.join("candidate.jsonl"), bytes));
    h.pointer(ready, false);
    assert!(
        h.copied.is_empty(),
        "new candidate modal must block the held background copy button"
    );
    assert!(h.app.copy_ready.is_some());
    h.click_rendered_label("取消");
    assert!(h.app.files.candidate.is_none());
    assert_eq!(h.app.entries.len(), 33);
    h.frame(vec![]);
    let ready = h
        .labels
        .iter()
        .find(|(label, _)| label == "复制已准备好")
        .unwrap()
        .1;
    h.pointer(ready, true);
    h.pointer(ready, false);
    assert_eq!(h.copied.len(), 1);
    assert!(h.copied[0] == expected);
}

#[test]
fn deep_filter_export_fails_without_publication_or_changing_the_open_document() {
    let directory = Directory::new();
    let input = directory.0.join("deep-source.jsonl");
    let output = directory.0.join("filtered.jsonl");
    let mut source = offline_filter_fixture();
    source.end.scope.kind = "filtered".into();
    let mut valid_bytes = None;
    for depth in 115..=132 {
        let mut nested = serde_json::Value::Null;
        for _ in 0..depth {
            nested = serde_json::Value::Array(vec![nested]);
        }
        source.end.scope.filter = Some(nested);
        let mut bytes = Vec::new();
        if document::write_snapshot(&mut bytes, &source, &AtomicBool::new(false)).is_ok()
            && source
                .select(&[0, 1, 2], serde_json::json!({"query":""}))
                .is_err()
        {
            valid_bytes = Some(bytes);
            break;
        }
    }
    let original = valid_bytes.expect("readable source whose composed filter exceeds the limit");
    fs::write(&input, &original).unwrap();
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    app.start_file_job(Action::Open, input.clone(), &ctx);
    wait_job(&mut app, &ctx);
    let opened = Arc::clone(app.files.document.as_ref().unwrap());
    app.start_file_job(Action::Export, output.clone(), &ctx);
    wait_job(&mut app, &ctx);
    assert!(!output.exists());
    assert!(
        app.files
            .notice
            .as_ref()
            .unwrap()
            .contains("recursion limit")
    );
    assert_eq!(fs::read(&input).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    assert!(Arc::ptr_eq(app.files.document.as_ref().unwrap(), &opened));
    assert!(Arc::ptr_eq(&app.entries, &opened.entries));
    assert_eq!(snapshot_bytes(&opened), original);
}

#[test]
fn failed_open_retains_path_for_explicit_keyboard_and_pointer_recovery() {
    let directory = Directory::new();
    let missing = directory.0.join("missing.jsonl");
    let valid = directory.0.join("valid.jsonl");
    fs::write(&valid, native(1, "opened after correction")).unwrap();
    let mut h = crate::ui_test_support::Harness::new(false);
    h.resize(900.0, 560.0);
    h.app.append_entry(LogEntry::marker("old view"));
    h.app.selected = Some(0);
    h.app.selection.select_all(&[0]);
    h.app.follow_logs = false;
    let old = Arc::clone(&h.app.entries);
    h.key(egui::Key::O, egui::Modifiers::COMMAND);
    h.frame(vec![egui::Event::Text(missing.display().to_string())]);
    h.key(egui::Key::Enter, egui::Modifiers::NONE);
    wait_job(&mut h.app, &h.ctx);
    h.frame(vec![]);
    assert!(
        h.app.files.dialog.is_none(),
        "failure must not interrupt with a modal"
    );
    assert!(
        h.app
            .files
            .notice
            .as_ref()
            .unwrap()
            .contains(&missing.display().to_string())
    );
    let (_, retry_bounds) = h
        .label_bounds
        .iter()
        .find(|(label, _)| label == "修改路径重试…")
        .unwrap();
    assert!(retry_bounds.max.x <= 900.0 && retry_bounds.max.y <= 560.0);
    h.click_rendered_label("修改路径重试…");
    assert_eq!(
        h.app.files.dialog.as_ref().unwrap().path,
        missing.display().to_string()
    );
    assert!(
        h.app.files.job.is_none(),
        "opening recovery must not retry automatically"
    );
    h.key(egui::Key::Escape, egui::Modifiers::NONE);
    assert!(Arc::ptr_eq(&old, &h.app.entries));
    assert_eq!(h.app.selected, Some(0));
    assert!(!h.app.follow_logs);
    // The ordinary shortcut is also useful recovery; Cancel did not lose the path.
    h.key(egui::Key::O, egui::Modifiers::COMMAND);
    assert_eq!(
        h.app.files.dialog.as_ref().unwrap().path,
        missing.display().to_string()
    );
    h.key(egui::Key::A, egui::Modifiers::COMMAND);
    h.frame(vec![egui::Event::Text(valid.display().to_string())]);
    h.key(egui::Key::Enter, egui::Modifiers::NONE);
    wait_job(&mut h.app, &h.ctx);
    h.frame(vec![]);
    assert!(h.app.files.candidate.is_some());
    assert!(
        Arc::ptr_eq(&old, &h.app.entries),
        "a corrected open still requires replacement confirmation"
    );
    h.click_rendered_label("直接替换");
    h.app.poll_files(&h.ctx);
    assert_eq!(h.app.entries[0].message.as_ref(), "opened after correction");
    assert!(h.app.files.retry.is_none());
}

#[test]
fn failed_save_before_open_retry_is_save_only_and_never_overwrites() {
    let directory = Directory::new();
    let source = directory.0.join("source.jsonl");
    let occupied = directory.0.join("occupied.jsonl");
    let corrected = directory.0.join("corrected.jsonl");
    fs::write(&source, native(1, "old evidence")).unwrap();
    fs::write(&occupied, b"leave existing destination unchanged").unwrap();
    let mut h = crate::ui_test_support::Harness::new(false);
    h.app.start_file_job(Action::Open, source, &h.ctx);
    wait_job(&mut h.app, &h.ctx);
    let old = Arc::clone(h.app.files.document.as_ref().unwrap());
    let candidate = document::load(
        io::Cursor::new(native(2, "must not open")),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    let bytes = candidate.memory_bytes();
    h.app.files.candidate = Some((candidate, directory.0.join("candidate.jsonl"), bytes));
    h.app
        .start_file_job(Action::SaveBeforeOpen, occupied.clone(), &h.ctx);
    wait_job(&mut h.app, &h.ctx);
    h.frame(vec![]);
    assert!(h.app.files.candidate.is_none());
    assert!(h.app.files.replace.is_none());
    assert_eq!(
        fs::read(&occupied).unwrap(),
        b"leave existing destination unchanged"
    );
    h.click_rendered_label("修改路径重试…");
    let dialog = h.app.files.dialog.as_ref().unwrap();
    assert!(dialog.action == Action::Save);
    assert_eq!(dialog.path, occupied.display().to_string());
    assert!(!corrected.exists());
    h.key(egui::Key::A, egui::Modifiers::COMMAND);
    h.frame(vec![egui::Event::Text(corrected.display().to_string())]);
    h.key(egui::Key::Enter, egui::Modifiers::NONE);
    wait_job(&mut h.app, &h.ctx);
    let saved = document::load(
        BufReader::new(File::open(&corrected).unwrap()),
        16 * MIB,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(saved.entries[0].message.as_ref(), "old evidence");
    assert!(Arc::ptr_eq(h.app.files.document.as_ref().unwrap(), &old));
    assert!(h.app.files.candidate.is_none());
    assert!(h.app.files.retry.is_none());
    assert_eq!(
        fs::read(&occupied).unwrap(),
        b"leave existing destination unchanged"
    );
}

#[test]
fn cancelled_file_failure_has_no_retry_and_dismiss_discards_failed_path() {
    let mut h = crate::ui_test_support::Harness::new(false);
    let (sender, receiver) = mpsc::channel();
    sender.send(Err("cancelled".into())).unwrap();
    h.app.files.job = Some(Job {
        cancel: Arc::new(AtomicBool::new(true)),
        receiver,
        path: PathBuf::from("cancelled.jsonl"),
        action: Action::Open,
    });
    h.app.poll_files(&h.ctx);
    h.frame(vec![]);
    assert!(h.app.files.retry.is_none());
    assert!(!h.labels.iter().any(|(label, _)| label == "修改路径重试…"));
    h.app.file_operation_failed(
        Action::Open,
        PathBuf::from("failed.jsonl"),
        "not found".into(),
        false,
    );
    h.frame(vec![]);
    h.click_rendered_label("×");
    assert!(h.app.files.notice.is_none());
    assert!(h.app.files.retry.is_none());
    h.key(egui::Key::O, egui::Modifiers::COMMAND);
    assert!(h.app.files.dialog.as_ref().unwrap().path.is_empty());
}

#[test]
fn export_path_recovery_waits_for_the_current_filter_before_confirmation() {
    let directory = Directory::new();
    let source = directory.0.join("source.jsonl");
    fs::write(&source, native(1, "retained evidence")).unwrap();
    let mut h = crate::ui_test_support::Harness::new(false);
    h.app.start_file_job(Action::Open, source, &h.ctx);
    wait_job(&mut h.app, &h.ctx);
    h.app.file_operation_failed(
        Action::Export,
        directory.0.join("export.jsonl"),
        "destination exists".into(),
        false,
    );
    h.app.query = "retained".into();
    h.app.queue_filter();
    h.frame(vec![]);
    h.click_rendered_label("修改路径重试…");
    assert!(
        h.app.files.dialog.is_none(),
        "debouncing filter must block export recovery just like File Export"
    );
    h.app.schedule_filter();
    h.frame(vec![]);
    h.click_rendered_label("修改路径重试…");
    assert!(
        h.app.files.dialog.is_none(),
        "pending filter must block export recovery"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.app.filter_pending {
        h.app.poll_filter();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    h.click_rendered_label("修改路径重试…");
    assert!(h.app.files.dialog.as_ref().unwrap().action == Action::Export);
    assert_eq!(h.app.active_filter.query, "retained");
    assert!(
        h.app.files.job.is_none(),
        "even ready export still requires confirmation"
    );
}

#[test]
fn file_recovery_notice_stays_compact_and_full_diagnostic_is_keyboard_accessible() {
    for width in [900.0, 1280.0] {
        for file_open in [false, true] {
            let directory = Directory::new();
            let mut h = crate::ui_test_support::Harness::new(false);
            h.ctx.enable_accesskit();
            h.resize(width, 560.0);
            if file_open {
                let source = directory.0.join("source.jsonl");
                fs::write(&source, native(1, "keep this source")).unwrap();
                h.app.start_file_job(Action::Open, source, &h.ctx);
                wait_job(&mut h.app, &h.ctx);
            } else {
                h.app.append_entry(LogEntry::marker("keep this view"));
            }
            h.app.selected = Some(0);
            h.app.selection.select_all(&[0]);
            h.app.show_details = false;
            let path = PathBuf::from(format!(
                "/synthetic/{}/evidence.jsonl",
                "long-path-".repeat(35)
            ));
            h.app.file_operation_failed(
                Action::Open,
                path.clone(),
                "Permission denied (os error 13)".into(),
                false,
            );
            let expected = h.app.files.notice.clone().unwrap();
            assert!(expected.starts_with("文件操作未完成：Permission denied (os error 13)"));
            h.frame(vec![]);
            h.frame(vec![]);
            let bounds = |label: &str| {
                // Compare interactive rectangles, not cross-font glyph ink boxes:
                // proportional × and CJK text have different optical centers.
                let rect = h
                    .accessible_nodes
                    .iter()
                    .find(|(_, node)| {
                        node.role() == egui::accesskit::Role::Button && node.label() == Some(label)
                    })
                    .and_then(|(_, node)| node.bounds())
                    .unwrap_or_else(|| panic!("missing button bounds: {label}"));
                egui::Rect::from_min_max(
                    egui::pos2(rect.x0 as f32, rect.y0 as f32),
                    egui::pos2(rect.x1 as f32, rect.y1 as f32),
                )
            };
            let retry = bounds("修改路径重试…");
            let details = bounds("详情");
            let dismiss = bounds("×");
            assert!((retry.center().y - details.center().y).abs() < 1.0);
            assert!((retry.center().y - dismiss.center().y).abs() < 1.0);
            assert!(retry.min.x >= 0.0 && dismiss.max.x <= width);
            assert!(retry.max.x < details.min.x && details.max.x < dismiss.min.x);
            assert!(
                dismiss.max.y <= if file_open { 148.0 } else { 122.0 },
                "file notice must not grow into three rows"
            );
            if file_open {
                assert!(
                    h.labels
                        .iter()
                        .any(|(label, _)| label.contains("文件 · source.jsonl · 1 条"))
                );
            }
            let following = h.app.follow_logs;
            h.click_rendered_label("详情");
            assert!(egui::Popup::is_id_open(
                &h.ctx,
                egui::Id::new("file_notice_diagnostic")
            ));
            let trigger = h.ctx.memory(|memory| memory.focused());
            let copy = h
                .labels
                .iter()
                .find(|(label, _)| label == "复制诊断")
                .unwrap()
                .1;
            h.pointer(copy, true);
            h.pointer(copy, false);
            assert_eq!(h.copied.as_slice(), std::slice::from_ref(&expected));
            assert_eq!(
                h.app.follow_logs, following,
                "diagnostic popup must not pause underlying table follow"
            );
            assert!(h.copied[0].contains(&path.display().to_string()));
            h.key(egui::Key::Escape, egui::Modifiers::NONE);
            assert_eq!(h.app.selected, Some(0));
            for _ in 0..60 {
                if h.ctx.memory(|memory| memory.focused()) == trigger {
                    break;
                }
                h.key(egui::Key::Tab, egui::Modifiers::NONE);
            }
            assert!(trigger.is_some());
            assert_eq!(h.ctx.memory(|memory| memory.focused()), trigger);
            h.key(egui::Key::Space, egui::Modifiers::NONE);
            assert!(egui::Popup::is_id_open(
                &h.ctx,
                egui::Id::new("file_notice_diagnostic")
            ));
            h.key(egui::Key::Escape, egui::Modifiers::NONE);
            assert_eq!(h.app.entries.len(), 1);
            assert_eq!(h.app.selected, Some(0));
        }
    }
}
