//! Typography role, metadata contrast and compact menu behavior regressions.
use super::ui_test_support::Harness;
use super::*;

#[test]
fn production_font_chain_separates_ui_and_logs_without_losing_cjk_or_symbols() {
    use egui::epaint::text::Fonts;

    let definitions = app_font_definitions();
    let defaults = egui::FontDefinitions::default();
    let ui = &definitions.families[&FontFamily::Proportional];
    let log = &definitions.families[&FontFamily::Monospace];
    assert_eq!(
        ui.first(),
        defaults.families[&FontFamily::Proportional].first()
    );
    assert_eq!(log.first().map(String::as_str), Some(APP_FONT_NAME));
    assert_eq!(
        &log[1..log.len() - 1],
        &defaults.families[&FontFamily::Monospace]
    );
    assert_eq!(ui.last().map(String::as_str), Some("Noto Sans Mono"));
    assert_eq!(log.last().map(String::as_str), Some("Noto Sans Mono"));

    let mut fonts = Fonts::new(Default::default(), definitions);
    let sample = "中文诊断 道路蹊跷 Il1 O0 10-29 09:00:00.006 PID 4242 /v1/payments 0xff ₿";
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        for size in [12.0, 12.5, 13.0] {
            let id = FontId::new(size, family.clone());
            for ch in sample.chars().filter(|ch| !ch.is_whitespace()) {
                assert!(fonts.has_glyph(&id, ch), "missing {ch} in {id:?}");
            }
            for scale in [1.0, 1.5] {
                let mut view = fonts.with_pixels_per_point(scale);
                let galley = view.layout_no_wrap(sample.into(), id.clone(), Color32::WHITE);
                assert_eq!(galley.job.text.as_str(), sample);
                let width = |text: &str, view: &mut egui::epaint::text::FontsView<'_>| {
                    view.layout_no_wrap(text.into(), id.clone(), Color32::WHITE)
                        .size()
                        .x
                };
                let (wide, narrow) = (width("WWWW", &mut view), width("iiii", &mut view));
                if family == FontFamily::Monospace {
                    assert!((wide - narrow).abs() < 0.1, "log advances changed");
                } else {
                    assert!(wide > narrow * 1.5, "UI must be proportional");
                }
                assert!(view.image().pixels.iter().any(|pixel| pixel.a() != 0));
            }
        }
    }
}

#[test]
fn important_metadata_has_contrast_without_changing_hint_or_disabled_colors() {
    fn luminance(color: Color32) -> f32 {
        let channel = |v: u8| {
            let value = f32::from(v) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }
    for dark in [true, false] {
        let ctx = egui::Context::default();
        configure_style(&ctx, dark);
        let visuals = ctx
            .style_of(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            })
            .visuals
            .clone();
        let defaults = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        assert_eq!(visuals.weak_text_color(), defaults.weak_text_color());
        let b = luminance(visuals.panel_fill);
        for color in [
            readable_metadata_color(&visuals),
            budget_warning_color(&visuals),
        ] {
            let a = luminance(color);
            assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
        }
        if dark {
            assert_eq!(
                budget_warning_color(&visuals),
                Color32::from_rgb(239, 190, 82)
            );
        }
    }
}

#[test]
fn settings_pages_fit_with_budget_warning_and_keyboard_dismissal_intact() {
    for dark in [true, false] {
        for size in [egui::vec2(900.0, 560.0), egui::vec2(1280.0, 820.0)] {
            let ctx = egui::Context::default();
            let app = CiallogcatApp::with_config(
                &ctx,
                AppConfig {
                    dark,
                    ..Default::default()
                },
                None,
            );
            let mut h = Harness::from_app(ctx, app, size);
            h.click_rendered_label("视图");
            for label in ["选中时显示详情", "快捷键…", "设置…"] {
                assert!(
                    h.labels.iter().any(|(text, _)| text == label),
                    "missing {label}"
                );
            }
            // Long-lived preferences no longer crowd the View menu.
            assert!(!h.labels.iter().any(|(text, _)| text == "日志内存预算"));
            h.key(egui::Key::Escape, egui::Modifiers::NONE);
            h.key(egui::Key::Comma, egui::Modifiers::COMMAND);
            assert!(h.app.show_settings);
            for (page, labels) in [
                ("外观", &["跟随系统", "深色", "浅色"][..]),
                ("日志阅读", &["选中时显示详情", "紧凑", "标准", "宽松"][..]),
                ("内存", &["日志内存预算", "应用预算", "取消修改"][..]),
            ] {
                h.click_rendered_label(page);
                for _ in 0..2 {
                    h.frame(vec![]);
                }
                for label in labels {
                    let (_, bounds) = h
                        .label_bounds
                        .iter()
                        .find(|(text, _)| text == label)
                        .unwrap_or_else(|| panic!("{page}: missing {label}"));
                    assert!(
                        bounds.min.y >= 0.0 && bounds.max.y <= size.y && bounds.max.x <= size.x,
                        "{label}: {bounds:?}"
                    );
                }
            }
            // A valid over-budget draft must not hide its warning or actions.
            for _ in 0..144 {
                h.app.append_entry(LogEntry::new(
                    "",
                    1,
                    1,
                    "Budget",
                    Level::Info,
                    "x".repeat(128 * 1024),
                ));
            }
            assert!(h.app.entries.memory_bytes() > memory_budget_bytes(16));
            h.app.memory_limit_draft = Some(16);
            for _ in 0..3 {
                h.frame(vec![]);
            }
            for label in [
                "应用后立即移除最旧日志，降至预算约 90%；无法撤销。",
                "应用预算",
                "取消修改",
            ] {
                let (_, bounds) = h
                    .label_bounds
                    .iter()
                    .find(|(text, _)| text == label)
                    .expect(label);
                assert!(
                    bounds.min.y >= 0.0 && bounds.max.y <= size.y - 8.0 && bounds.max.x <= size.x,
                    "{label}: {bounds:?}"
                );
            }
            h.key(egui::Key::Escape, egui::Modifiers::NONE);
            assert!(!h.app.show_settings);
            assert!(h.app.memory_limit_draft.is_none());
            assert_eq!(h.app.memory_limit_mib, 300);
            h.key(egui::Key::Comma, egui::Modifiers::COMMAND);
            for _ in 0..2 {
                h.frame(vec![]);
            }
            assert_eq!(h.app.memory_limit_draft, Some(300));
        }
    }
}

#[test]
fn theme_choice_follows_system_and_keeps_legacy_dark_configs() {
    let legacy: AppConfig = serde_json::from_str(r#"{"dark":false}"#).unwrap();
    assert_eq!(legacy.theme_choice(), ThemeChoice::Light);
    assert_eq!(AppConfig::default().theme_choice(), ThemeChoice::Dark);

    let ctx = egui::Context::default();
    let app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
    let mut h = Harness::from_app(ctx, app, egui::vec2(1280.0, 820.0));
    h.key(egui::Key::Comma, egui::Modifiers::COMMAND);
    h.click_rendered_label("外观");
    h.click_rendered_label("浅色");
    assert_eq!(h.app.theme, ThemeChoice::Light);
    assert!(!h.app.dark);
    assert!(!h.ctx.global_style().visuals.dark_mode);
    assert_eq!(h.app.app_config().theme, Some(ThemeChoice::Light));
    h.click_rendered_label("跟随系统");
    assert_eq!(h.app.theme, ThemeChoice::System);
    for system in [egui::Theme::Light, egui::Theme::Dark] {
        let mut output = h.ctx.run_ui(
            egui::RawInput {
                system_theme: Some(system),
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        h.app.sync_theme(&h.ctx);
        assert_eq!(h.app.dark, system == egui::Theme::Dark);
        assert_eq!(
            h.ctx.global_style().visuals.dark_mode,
            system == egui::Theme::Dark
        );
        assert_eq!(
            h.ctx.global_style().spacing.item_spacing,
            egui::vec2(8.0, 6.0)
        );
    }
}

#[test]
fn shortcut_help_lists_every_shortcut_within_the_smallest_window() {
    for size in [egui::vec2(900.0, 560.0), egui::vec2(1280.0, 820.0)] {
        let ctx = egui::Context::default();
        let app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        let mut h = Harness::from_app(ctx, app, size);
        h.click_rendered_label("视图");
        h.click_rendered_label("快捷键…");
        assert!(h.app.show_shortcuts);
        for _ in 0..3 {
            h.frame(vec![]);
        }
        for (keys, action) in SHORTCUTS {
            for label in [*keys, *action] {
                let (_, bounds) = h
                    .label_bounds
                    .iter()
                    .find(|(text, _)| text == label)
                    .unwrap_or_else(|| panic!("missing {label}"));
                assert!(
                    bounds.min.x >= 0.0
                        && bounds.min.y >= 0.0
                        && bounds.max.x <= size.x
                        && bounds.max.y <= size.y,
                    "{label}: {bounds:?}"
                );
            }
        }
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.show_shortcuts);
        h.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(h.app.show_shortcuts);
        h.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(!h.app.show_shortcuts);
    }
}

#[test]
fn english_toolbar_settings_and_switching_fit_the_smallest_window() {
    let ctx = egui::Context::default();
    let app = CiallogcatApp::with_config(
        &ctx,
        AppConfig {
            language: Some(Language::En),
            ..Default::default()
        },
        None,
    );
    let mut h = Harness::from_app(ctx, app, egui::vec2(900.0, 560.0));
    let row: Vec<String> = [
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
    ]
    .into_iter()
    .map(|key| tr!(key))
    .collect();
    assert_eq!(row[2], "Start capture");
    let bounds = |h: &Harness, label: &str| {
        h.label_bounds
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("missing {label}: {:?}", h.labels))
            .1
    };
    for label in row
        .iter()
        .map(String::as_str)
        .chain([".*", "Aa", "Tag", "V", "F"])
    {
        let rect = bounds(&h, label);
        assert!(
            rect.min.x >= 0.0 && rect.max.x <= 900.0 && rect.min.y >= 0.0 && rect.max.y <= 90.0,
            "{label}: {rect:?}"
        );
    }
    for (index, left) in row.iter().enumerate() {
        for right in &row[index + 1..] {
            assert!(
                !bounds(&h, left).intersects(bounds(&h, right)),
                "toolbar labels overlap: {left}, {right}"
            );
        }
    }
    assert_toolbar_frames_do_not_overlap(&h);
    h.app.follow_logs = false;
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.labels.iter().any(|(text, _)| text == "↓ Latest"));
    assert_toolbar_frames_do_not_overlap(&h);
    for hint in ["Package · all processes", "Filter messages · / to focus"] {
        assert!(
            h.labels.iter().any(|(text, _)| text == hint),
            "missing {hint}"
        );
    }

    // Every settings page fits in English too.
    h.key(egui::Key::Comma, egui::Modifiers::COMMAND);
    for (page, labels) in [
        (
            "Appearance",
            &["语言 / Language", "English", "简体中文", "Follow system"][..],
        ),
        (
            "Reading",
            &["Show details on selection", "Compact", "Relaxed"][..],
        ),
        (
            "Memory",
            &["Log memory budget", "Apply budget", "Discard changes"][..],
        ),
    ] {
        h.click_rendered_label(page);
        for label in labels {
            let rect = bounds(&h, label);
            assert!(
                rect.max.x <= 900.0 && rect.max.y <= 560.0,
                "{label}: {rect:?}"
            );
        }
    }

    // Switching language applies immediately and is saved.
    h.click_rendered_label("Appearance");
    h.click_rendered_label("简体中文");
    assert_eq!(h.app.language, Language::ZhCn);
    assert_eq!(h.app.app_config().language, Some(Language::ZhCn));
    h.click_rendered_label("关闭");
    for _ in 0..2 {
        h.frame(vec![]);
    }
    assert!(h.labels.iter().any(|(text, _)| text == "开始采集"));
    assert!(!h.labels.iter().any(|(text, _)| text == "Start capture"));
    assert_toolbar_frames_do_not_overlap(&h);
    h.app.follow_logs = true;
    h.frame(vec![]);
    h.frame(vec![]);
    assert_toolbar_frames_do_not_overlap(&h);
}

/// Button and combo frames painted in the first toolbar row must not overlap;
/// text bounds alone miss a combo arrow hidden under the next button.
fn assert_toolbar_frames_do_not_overlap(h: &Harness) {
    let mut frames = Vec::new();
    fn collect(shape: &egui::Shape, frames: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect)
                if rect.rect.max.y <= 44.0
                    && (18.0..=34.0).contains(&rect.rect.height())
                    && rect.rect.width() < 300.0 =>
            {
                frames.push(rect.rect)
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, frames)),
            _ => {}
        }
    }
    for shape in &h.shapes {
        collect(&shape.shape, &mut frames);
    }
    assert!(
        frames.len() >= 8,
        "found only {} toolbar frames",
        frames.len()
    );
    for (index, a) in frames.iter().enumerate() {
        assert!(
            a.min.x >= 0.0 && a.max.x <= h.ctx.content_rect().max.x,
            "{a:?}"
        );
        for b in &frames[index + 1..] {
            assert!(
                a.intersect(*b).width() <= 0.5 || a == b,
                "toolbar frames overlap: {a:?} {b:?}"
            );
        }
    }
}
