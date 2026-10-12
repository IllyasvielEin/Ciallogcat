//! Clear confirmation must not turn modal presses into background browsing.
use super::ui_test_support::Harness;
use super::*;

fn setup(following: bool) -> Harness {
    let ctx = egui::Context::default();
    let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    for index in 0..40 {
        app.append_entry(LogEntry::marker(format!("clear-test-{index}")));
    }
    app.selected = Some(2);
    app.selection.select(&app.matches, 2, false, false);
    app.follow_logs = following;
    app.show_details = false;
    Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0))
}

#[test]
fn clear_cancel_escape_and_backdrop_preserve_both_original_follow_states() {
    for following in [true, false] {
        for (dismissal, device_switch) in [(0, false), (1, false), (2, false), (0, true)] {
            let mut h = setup(following);
            if device_switch {
                h.app
                    .request_device_switch("other-test-device".to_owned(), &h.ctx);
                h.frame(vec![]);
                h.frame(vec![]);
                assert!(h.app.pending_device.is_some());
            } else {
                h.click_rendered_label("Clear");
                assert!(h.app.pending_clear);
            }
            assert_eq!(h.app.follow_logs, following);
            // Capture may continue while the confirmation is visible.
            h.app
                .append_entry(LogEntry::marker("arrived while confirming"));
            match dismissal {
                0 | 2 => {
                    let pos = if dismissal == 0 {
                        h.labels
                            .iter()
                            .find(|(label, _)| label == "Cancel")
                            .unwrap()
                            .1
                    } else {
                        egui::pos2(40.0, 620.0)
                    };
                    h.pointer(pos, true);
                    assert_eq!(
                        h.app.follow_logs, following,
                        "modal press must not pause background follow"
                    );
                    h.pointer(pos, false);
                    h.frame(vec![]);
                }
                1 => h.key(egui::Key::Escape, egui::Modifiers::NONE),
                _ => unreachable!(),
            }
            assert!(!h.app.pending_clear);
            assert!(h.app.pending_device.is_none());
            assert_eq!(h.app.follow_logs, following);
            assert_eq!(h.app.entries.len(), 41);
            assert_eq!(h.app.selected, Some(2));
            assert!(h.app.selection.contains(2));
            h.app.append_entry(LogEntry::marker("after cancellation"));
            h.frame(vec![]);
            assert_eq!(h.app.follow_logs, following);
        }
    }
}

#[test]
fn clear_confirmation_still_clears_without_changing_capture_or_follow_choice() {
    for following in [true, false] {
        let mut h = setup(following);
        h.click_rendered_label("Clear");
        let pos = h
            .labels
            .iter()
            .find(|(label, _)| label == "清空并继续")
            .unwrap()
            .1;
        h.pointer(pos, true);
        assert_eq!(h.app.follow_logs, following);
        h.pointer(pos, false);
        h.frame(vec![]);
        assert!(h.app.entries.is_empty());
        assert!(h.app.matches.is_empty());
        assert!(h.app.selection.is_empty());
        assert!(h.app.selected.is_none());
        assert!(!h.app.pending_clear);
        assert!(matches!(h.app.capture_state, CaptureState::NoAdb));
        assert_eq!(h.app.follow_logs, following);
        h.app.append_entry(LogEntry::marker("capture may continue"));
        h.frame(vec![]);
        assert_eq!(h.app.entries.len(), 1);
        assert_eq!(h.app.follow_logs, following);
    }
}
