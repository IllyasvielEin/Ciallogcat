//! Settings dialog: long-lived preferences moved out of the toolbar menus.
//!
//! Appearance and reading preferences apply immediately and are saved with the
//! usual debounce. The memory budget keeps its explicit draft/apply flow because
//! lowering it irreversibly trims retained logs.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Page {
    Appearance,
    Reading,
    Memory,
}

impl Page {
    const ALL: [Self; 3] = [Self::Appearance, Self::Reading, Self::Memory];

    fn label(self) -> String {
        match self {
            Self::Appearance => tr!("外观"),
            Self::Reading => tr!("日志阅读"),
            Self::Memory => tr!("内存"),
        }
    }
}

const BODY_HEIGHT: f32 = 300.0;
const ROW_HEIGHT_PRESETS: [(&str, f32); 3] = [("紧凑", 22.0), ("标准", 25.0), ("宽松", 30.0)];

impl CiallogcatApp {
    pub(super) fn open_settings(&mut self, page: Option<Page>) {
        if let Some(page) = page {
            self.settings_page = page;
        }
        self.memory_limit_draft = None;
        self.show_settings = true;
    }

    fn close_settings(&mut self) {
        self.show_settings = false;
        self.memory_limit_draft = None;
    }

    /// Resolve the effective palette; "follow system" can change between frames.
    pub(super) fn sync_theme(&mut self, ctx: &egui::Context) {
        self.dark = match self.theme {
            ThemeChoice::System => ctx.theme() == egui::Theme::Dark,
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
        };
    }

    fn set_theme(&mut self, ctx: &egui::Context, theme: ThemeChoice) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        apply_theme(ctx, theme);
        self.sync_theme(ctx);
        self.mark_config_dirty();
    }

    pub(super) fn settings_dialog(&mut self, ctx: &egui::Context) {
        let mut close =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut theme = self.theme;
        let mut language = self.language;
        let response = egui::Modal::new(egui::Id::new("settings_dialog")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(tr!("设置"));
            ui.add_space(6.0);
            // Fixed body box: a vertical separator fills the available height,
            // so an unbounded row would grow the modal on every frame.
            ui.allocate_ui_with_layout(
                egui::vec2(560.0, BODY_HEIGHT),
                Layout::left_to_right(Align::Min),
                |ui| {
                    ui.set_height(BODY_HEIGHT);
                    ui.allocate_ui_with_layout(
                        egui::vec2(112.0, BODY_HEIGHT),
                        Layout::top_down_justified(Align::Min),
                        |ui| {
                            for page in Page::ALL {
                                if ui
                                    .selectable_label(self.settings_page == page, page.label())
                                    .clicked()
                                {
                                    self.settings_page = page;
                                }
                            }
                        },
                    );
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), BODY_HEIGHT),
                        Layout::top_down(Align::Min),
                        |ui| match self.settings_page {
                            Page::Appearance => appearance_page(ui, &mut language, &mut theme),
                            Page::Reading => self.reading_page(ui),
                            Page::Memory => self.memory_page(ui),
                        },
                    );
                },
            );
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(tr!("键盘快捷键…")).shortcut_text("F1"))
                    .clicked()
                {
                    self.show_shortcuts = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close |= ui.button(tr!("关闭")).clicked();
                });
            });
        });
        self.set_theme(ctx, theme);
        if language != self.language {
            self.language = language;
            i18n::set(language);
            self.mark_config_dirty();
        }
        if self.settings_page != Page::Memory {
            self.memory_limit_draft = None;
        }
        // Opening shortcut help draws over Settings; its backdrop is not ours.
        if close || (response.should_close() && !self.show_shortcuts) {
            self.close_settings();
        }
    }

    fn reading_page(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.show_details, tr!("选中时显示详情"))
            .changed()
        {
            self.mark_config_dirty();
        }
        ui.add_space(10.0);
        ui.label(RichText::new(tr!("日志行高")).strong());
        ui.horizontal(|ui| {
            for (label, height) in ROW_HEIGHT_PRESETS {
                if ui
                    .selectable_label((self.row_height - height).abs() < 0.5, tr!(label))
                    .clicked()
                {
                    self.row_height = height;
                    self.mark_config_dirty();
                }
            }
        });
        if ui
            .add(egui::Slider::new(&mut self.row_height, 21.0..=34.0).suffix(" px"))
            .changed()
        {
            self.mark_config_dirty();
        }
    }

    fn memory_page(&mut self, ui: &mut egui::Ui) {
        let mut draft = self.memory_limit_draft.unwrap_or(self.memory_limit_mib);
        let value = ui
            .horizontal(|ui| {
                ui.label(tr!("日志内存预算"));
                ui.add(
                    egui::DragValue::new(&mut draft)
                        .range(16..=4096)
                        .suffix(" MiB"),
                )
            })
            .inner;
        self.memory_limit_draft = Some(draft);
        ui.add(
            egui::Label::new(tr!("仅估算保留日志；不含快照、队列、剪贴板及进程总内存。")).wrap(),
        );
        ui.label(
            RichText::new(tr!(
                "当前保留 ≈ %{used} MiB",
                used = self.entries.memory_bytes() as f64 / MIB as f64 : {:.1}
            ))
            .weak(),
        );
        if self.entries.memory_bytes() > memory_budget_bytes(draft) {
            ui.colored_label(
                budget_warning_color(ui.visuals()),
                tr!("应用后立即移除最旧日志，降至预算约 90%；无法撤销。"),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    draft != self.memory_limit_mib,
                    egui::Button::new(tr!("应用预算")),
                )
                .clicked()
            {
                self.apply_memory_budget(draft);
                self.memory_limit_draft = None;
            }
            if ui.button(tr!("取消修改")).clicked() {
                // Discard an in-progress text edit too; otherwise losing focus
                // next frame would write the cancelled number back.
                ui.data_mut(|data| data.remove_temp::<String>(value.id));
                ui.memory_mut(|memory| memory.surrender_focus(value.id));
                self.memory_limit_draft = None;
            }
        });
    }
}

fn appearance_page(ui: &mut egui::Ui, language: &mut Language, theme: &mut ThemeChoice) {
    // Bilingual label: findable whichever language is currently shown.
    ui.label(RichText::new(tr!("语言 / Language")).strong());
    ui.horizontal(|ui| {
        for option in Language::ALL {
            ui.selectable_value(language, option, option.native_name());
        }
    });
    ui.add_space(10.0);
    ui.label(RichText::new(tr!("主题")).strong());
    ui.horizontal(|ui| {
        ui.selectable_value(theme, ThemeChoice::System, tr!("跟随系统"));
        ui.selectable_value(theme, ThemeChoice::Dark, tr!("深色"));
        ui.selectable_value(theme, ThemeChoice::Light, tr!("浅色"));
    });
    ui.label(RichText::new(tr!("跟随系统时，随操作系统的深浅色设置即时切换。")).weak());
}
