//! Bounded toolbar/status layout and semantics from real egui frames.
use super::ui_test_support::Harness;
use super::*;

fn harness(dark: bool) -> Harness {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = CiallogcatApp::with_config(
        &ctx,
        AppConfig {
            dark,
            ..Default::default()
        },
        None,
    );
    app.devices = vec![DeviceInfo {
        serial: "audit-device".into(),
        model: Some("Samsung_Galaxy_S24_Ultra_Enterprise_Edition".into()),
        state: "device".into(),
        transport: "USB",
    }];
    app.selected_device = Some("audit-device".into());
    app.capture_state = CaptureState::Paused;
    app.append_entry(LogEntry::new(
        "10-11 13:00:01.123",
        12345,
        45678,
        "Audit",
        Level::Warn,
        "selected evidence",
    ));
    app.selected = Some(0);
    app.selection.select(&app.matches, 0, false, false);
    Harness::from_app(ctx, app, egui::vec2(900.0, 560.0))
}
fn bounded(h: &Harness, label: &str) {
    let (_, bounds) = h
        .label_bounds
        .iter()
        .find(|(text, _)| text == label)
        .unwrap_or_else(|| panic!("missing {label:?}"));
    assert!(
        bounds.min.x >= 0.0
            && bounds.max.x <= 900.0
            && bounds.min.y >= 0.0
            && bounds.max.y <= 560.0,
        "{label}: {bounds:?}"
    );
}

#[test]
fn long_device_names_leave_all_filter_controls_inside_minimum_window() {
    for dark in [true, false] {
        let mut h = harness(dark);
        for model in [
            "Samsung_Galaxy_S24_Ultra_Enterprise_Edition".to_owned(),
            "开发测试设备".repeat(100),
        ] {
            h.app.devices[0].model = Some(model);
            h.frame(vec![]);
            h.frame(vec![]);
            for label in ["筛选方案", ".*", "Aa", "↻ 刷新"] {
                bounded(&h, label);
            }
            for id in ["package_filter", "query_shortcut"] {
                let rect = h.ctx.read_response(egui::Id::new(id)).unwrap().rect;
                assert!(rect.min.x >= 0.0 && rect.max.x <= 900.0, "{id}: {rect:?}");
                assert!(rect.width() >= 100.0);
            }
        }
        // The complete model remains represented for assistive technology despite visual elision.
        let full = h.app.devices[0].display_name();
        assert!(
            h.accessible_nodes
                .iter()
                .any(|(_, node)| node.value() == Some(full.as_str()))
        );
    }
}

#[test]
fn simultaneous_diagnostics_preserve_counts_and_complete_copy_with_escape() {
    for dark in [true, false] {
        let mut h = harness(dark);
        let capture = format!(
            "adb capture: {} END-CAPTURE",
            "网络异常 diagnostic ".repeat(180)
        );
        let settings = format!(
            "Failed to save settings: {} END-SETTINGS",
            "permission denied ".repeat(80)
        );
        let adb = "ADB device query failed: END-QUERY".to_owned();
        h.app.capture_state = CaptureState::Error(capture.clone());
        h.app.config_error = Some(settings.clone());
        h.app.package_query_error = Some(adb.clone());
        h.frame(vec![]);
        for label in [
            "采集错误",
            "设置保存失败",
            "包列表查询错误",
            "1 条日志 · 0/秒",
        ] {
            bounded(&h, label);
        }
        for (label, popup, expected) in [
            ("采集错误", "capture_diagnostic", capture),
            ("设置保存失败", "settings_diagnostic", settings),
            ("包列表查询错误", "package_diagnostic", adb),
        ] {
            h.click_label(label);
            assert!(egui::Popup::is_id_open(&h.ctx, egui::Id::new(popup)));
            let trigger_focus = h.ctx.memory(|memory| memory.focused());
            let pos = h
                .labels
                .iter()
                .find(|(text, _)| text == "复制诊断")
                .unwrap()
                .1;
            h.pointer(pos, true);
            h.pointer(pos, false);
            assert_eq!(h.copied, [expected]);
            h.key(egui::Key::Escape, egui::Modifiers::NONE);
            assert!(!egui::Popup::is_id_open(&h.ctx, egui::Id::new(popup)));
            assert_eq!(h.app.selected, Some(0));
            // The trigger remains reachable through ordinary keyboard traversal.
            for _ in 0..60 {
                if h.ctx.memory(|memory| memory.focused()) == trigger_focus {
                    break;
                }
                h.key(egui::Key::Tab, egui::Modifiers::NONE);
            }
            assert!(trigger_focus.is_some());
            assert_eq!(h.ctx.memory(|memory| memory.focused()), trigger_focus);
            h.key(egui::Key::Space, egui::Modifiers::NONE);
            assert!(egui::Popup::is_id_open(&h.ctx, egui::Id::new(popup)));
            h.key(egui::Key::Escape, egui::Modifiers::NONE);
        }
    }
}

#[test]
fn filter_controls_have_meaningful_accessible_names_and_states() {
    for dark in [true, false] {
        let mut h = harness(dark);
        for name in [
            "使用正则表达式",
            "区分大小写",
            "包名筛选",
            "消息筛选",
            "最低级别：Verbose 及以上",
        ] {
            assert!(
                h.accessible_nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(name)),
                "missing {name}"
            );
        }
        h.click_label(".*");
        assert!(h.app.use_regex);
        let node = h
            .accessible_nodes
            .iter()
            .find(|(_, node)| node.label() == Some("使用正则表达式"))
            .unwrap();
        assert_eq!(node.1.toggled(), Some(egui::accesskit::Toggled::True));
        h.key(egui::Key::Space, egui::Modifiers::NONE);
        assert!(
            !h.app.use_regex,
            "keyboard activation should still work with semantic labels"
        );
        for level in Level::ALL {
            assert_eq!(
                selected_level_color(level, dark, true),
                if dark { Color32::WHITE } else { Color32::BLACK }
            );
        }
    }
}

#[test]
fn diagnostic_button_text_keeps_contrast_in_both_themes_and_interaction_states() {
    fn luminance(c: Color32) -> f64 {
        let s = c.to_array();
        let f = |v: u8| {
            let x = f64::from(v) / 255.;
            if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(s[0]) + 0.7152 * f(s[1]) + 0.0722 * f(s[2])
    }
    for dark in [true, false] {
        let ctx = egui::Context::default();
        configure_style(&ctx, dark);
        configure_fonts(&ctx);
        let mut pos = egui::pos2(30., 15.);
        for state in ["inactive", "hovered", "focused", "pressed"] {
            let events = match state {
                "inactive" => vec![egui::Event::PointerGone],
                "hovered" => vec![egui::Event::PointerMoved(pos)],
                "focused" => vec![
                    egui::Event::PointerGone,
                    egui::Event::Key {
                        key: egui::Key::Tab,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                _ => vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            };
            let mut result = None;
            for i in 0..3 {
                let mut out = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900., 560.),
                        )),
                        events: if i == 0 { events.clone() } else { vec![] },
                        ..Default::default()
                    },
                    |ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            CiallogcatApp::status_diagnostic(
                                ui,
                                "review_contrast",
                                "采集错误",
                                "fixture error",
                            );
                        });
                    },
                );
                out.textures_delta.clear();
                fn flatten<'a>(s: &'a egui::Shape, v: &mut Vec<&'a egui::Shape>) {
                    if let egui::Shape::Vec(ss) = s {
                        for s in ss {
                            flatten(s, v)
                        }
                    } else {
                        v.push(s)
                    }
                }
                let mut shapes = vec![];
                for s in &out.shapes {
                    flatten(&s.shape, &mut shapes)
                }
                let t = shapes
                    .iter()
                    .find_map(|s| {
                        if let egui::Shape::Text(t) = s {
                            (t.galley.text() == "采集错误").then_some(t)
                        } else {
                            None
                        }
                    })
                    .expect("painted diagnostic label");
                pos = t.visual_bounding_rect().center();
                let bg = shapes
                    .iter()
                    .filter_map(|s| {
                        if let egui::Shape::Rect(r) = s {
                            (r.rect.contains(pos) && r.fill.a() == 255).then_some(r)
                        } else {
                            None
                        }
                    })
                    .min_by(|a, b| a.rect.area().total_cmp(&b.rect.area()))
                    .expect("opaque button background")
                    .fill;
                let color = t.override_text_color.unwrap_or_else(|| {
                    let c = t.galley.job.sections[0].format.color;
                    if c == Color32::PLACEHOLDER {
                        t.fallback_color
                    } else {
                        c
                    }
                });
                let a = luminance(color);
                let b = luminance(bg);
                let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                result = Some((color, bg, contrast));
            }
            let (color, bg, contrast) = result.unwrap();
            println!(
                "diagnostic dark={dark} {state} fg={color:?} bg={bg:?} contrast={contrast:.3}"
            );
            assert!(
                contrast >= 4.5,
                "diagnostic dark={dark} state={state}: {contrast}"
            );
        }
    }
}
