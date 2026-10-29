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
fn view_help_expands_with_budget_controls_and_keyboard_dismissal_intact() {
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
            for label in [
                "日志内存预算",
                "应用预算",
                "取消修改",
                "选中时显示详情",
                "深色主题",
                "快捷键",
            ] {
                assert!(
                    h.labels.iter().any(|(text, _)| text == label),
                    "missing {label}"
                );
            }
            assert!(!h.labels.iter().any(|(text, _)| text == "聚焦消息筛选"));
            h.click_rendered_label("快捷键");
            for _ in 0..20 {
                h.frame(vec![]);
            }
            for label in ["应用预算", "取消修改", "聚焦消息筛选", "退出输入或选择"]
            {
                let (_, bounds) = h
                    .label_bounds
                    .iter()
                    .find(|(text, _)| text == label)
                    .expect(label);
                assert!(
                    bounds.min.x >= 0.0 && bounds.max.x <= size.x && bounds.max.y <= size.y,
                    "{label}: {bounds:?}"
                );
            }
            let first = h
                .label_bounds
                .iter()
                .find(|(text, _)| text == "聚焦消息筛选")
                .unwrap()
                .1;
            let last = h
                .label_bounds
                .iter()
                .find(|(text, _)| text == "退出输入或选择")
                .unwrap()
                .1;
            assert!(
                last.max.y - first.min.y <= 168.0,
                "seven help rows must remain compact"
            );
            // A valid over-budget draft must not hide its warning or final help row.
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
                "退出输入或选择",
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
            assert!(h.app.memory_limit_draft.is_none());
            assert_eq!(h.app.memory_limit_mib, 300);
            h.click_rendered_label("视图");
            assert_eq!(h.app.memory_limit_draft, Some(300));
        }
    }
}
