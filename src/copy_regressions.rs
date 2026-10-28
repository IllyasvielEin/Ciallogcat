use super::*;
use ui_test_support::Harness;

fn fixture() -> Harness {
    let mut h = Harness::new(false);
    h.resize(900.0, 560.0);
    for index in 0..400 {
        h.app
            .append_entry(LogEntry::marker(format!("{index} {}", "日志".repeat(100))));
    }
    h.app.selection.select_all(&h.app.matches);
    h.app.selected = Some(0);
    h.frame(vec![]);
    h
}

fn wait_ready(h: &mut Harness) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while h.app.copy_ready.is_none() {
        h.frame(vec![]);
        assert!(
            h.copied.is_empty(),
            "completion must never write clipboard automatically"
        );
        assert!(Instant::now() < deadline, "copy preparation did not finish");
        std::thread::yield_now();
    }
    h.frame(vec![]);
}

#[test]
fn large_copy_requires_explicit_commit_and_freezes_selection_during_append() {
    let mut h = fixture();
    let expected = h.app.selection.copy_text(&h.app.entries);
    h.frame(vec![egui::Event::Copy]);
    assert!(h.copied.is_empty());
    assert!(h.app.copy_pending.is_some());
    h.app
        .append_entry(LogEntry::marker("new tail must not join frozen selection"));
    wait_ready(&mut h);
    for label in ["复制已准备好", "取消复制"] {
        let rect = h
            .label_bounds
            .iter()
            .find(|(text, _)| text == label)
            .unwrap()
            .1;
        assert!(rect.min.x >= 0.0 && rect.max.x <= 900.0 && rect.max.y <= 560.0);
    }
    let pos = h
        .labels
        .iter()
        .find(|(text, _)| text == "复制已准备好")
        .unwrap()
        .1;
    h.pointer(pos, true);
    h.pointer(pos, false);
    assert_eq!(h.copied.len(), 1);
    assert!(
        h.copied[0] == expected,
        "copy must equal the entire original selection"
    );
    assert!(h.app.copy_pending.is_none());
    assert!(h.app.copy_ready.is_none());
}

#[test]
fn selection_filter_trim_and_source_changes_reject_prepared_results() {
    for change in 0..4 {
        let mut h = fixture();
        h.frame(vec![egui::Event::Copy]);
        wait_ready(&mut h);
        match change {
            0 => {
                h.app.selection.select(&h.app.matches, 1, false, false);
            }
            1 => {
                h.app.query = "new filter".into();
                h.app.queue_filter();
            }
            2 => {
                Arc::make_mut(&mut h.app.entries).drain_front(1);
                h.app.rebase_after_trim(1);
            }
            _ => {
                h.app.clear_logs();
            }
        }
        let mut output = h.ctx.run_ui(egui::RawInput::default(), |_ui| {
            h.app.commit_prepared_copy(&h.ctx)
        });
        output.textures_delta.clear();
        assert!(
            !output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        );
        assert!(h.app.copy_pending.is_none());
        assert!(h.app.copy_ready.is_none());
    }
}

#[test]
fn newer_copy_cancel_button_and_text_copy_supersede_old_preparation() {
    let mut h = fixture();
    h.frame(vec![egui::Event::Copy]);
    let old = h.app.copy_pending.unwrap().0;
    h.frame(vec![egui::Event::Copy]);
    assert_ne!(h.app.copy_pending.unwrap().0, old);
    wait_ready(&mut h);
    h.click_label("取消复制");
    assert!(h.copied.is_empty());
    assert!(h.app.copy_pending.is_none());
    h.frame(vec![egui::Event::Copy]);
    wait_ready(&mut h);
    let output = h.ctx.run_ui(egui::RawInput::default(), |_ui| {
        h.ctx.copy_text("newer selected text".into());
        h.app.copy_shortcut(&h.ctx);
    });
    assert!(h.app.copy_pending.is_none());
    assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == "newer selected text")));
}

#[test]
fn background_oversize_never_publishes_partial_clipboard_text() {
    let mut h = Harness::new(false);
    for _ in 0..300 {
        h.app.append_entry(LogEntry::marker("x".repeat(64 * 1024)));
    }
    h.app.selection.select_all(&h.app.matches);
    h.frame(vec![egui::Event::Copy]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while h.app.copy_pending.is_some() {
        h.frame(vec![]);
        assert!(h.copied.is_empty());
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(h.app.copy_ready.is_none());
    assert!(
        h.app
            .copy_notice
            .as_ref()
            .unwrap()
            .contains("clipboard limit")
    );
}
