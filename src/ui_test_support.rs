//! Shared real-frame pointer/keyboard harness; no new tests.
use super::*;

pub(super) struct Harness {
    pub(super) ctx: egui::Context,
    pub(super) app: CiallogcatApp,
    pub(super) labels: Vec<(String, egui::Pos2)>,
    pub(super) copied: Vec<String>,
    pub(super) label_bounds: Vec<(String, egui::Rect)>,
    pub(super) accessible_nodes: Vec<(egui::accesskit::NodeId, egui::accesskit::Node)>,
    pub(super) shapes: Vec<egui::epaint::ClippedShape>,
    screen_size: egui::Vec2,
}

impl Harness {
    pub(super) fn new(populated: bool) -> Self {
        let ctx = egui::Context::default();
        let config = AppConfig {
            filter_history: if populated {
                vec!["timeout".to_owned(), "fatal".to_owned()]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let mut app = CiallogcatApp::with_config(&ctx, config, None);
        app.packages = vec!["com.example.app".to_owned(), "com.example.other".to_owned()];
        Self::from_app(ctx, app, egui::vec2(1280.0, 820.0))
    }

    pub(super) fn from_app(
        ctx: egui::Context,
        app: CiallogcatApp,
        screen_size: egui::Vec2,
    ) -> Self {
        let mut harness = Self {
            ctx,
            app,
            labels: Vec::new(),
            copied: Vec::new(),
            label_bounds: Vec::new(),
            accessible_nodes: Vec::new(),
            shapes: Vec::new(),
            screen_size,
        };
        harness.frame(vec![]);
        harness.frame(vec![]);
        harness
    }

    pub(super) fn resize(&mut self, width: f32, height: f32) {
        self.screen_size = egui::vec2(width, height);
        self.frame(vec![]);
        self.frame(vec![]);
    }

    pub(super) fn frame(&mut self, events: Vec<egui::Event>) {
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    self.screen_size,
                )),
                events,
                ..Default::default()
            },
            |ui| {
                self.app.file_shortcuts(&self.ctx);
                self.app.shortcuts(&self.ctx);
                if self.app.files.is_modal()
                    || self.app.pending_clear
                    || self.app.pending_device.is_some()
                    || self.app.show_save_filter
                {
                    ui.disable();
                }
                self.app.toolbar(ui);
                self.app.file_notice(ui);
                self.app.context_notice(ui);
                self.app.filter_notice(ui);
                self.app.status_bar(ui);
                self.app.details(ui);
                self.app.log_table(ui);
                self.app.dialogs(&self.ctx);
                self.app.copy_shortcut(&self.ctx);
            },
        );
        self.copied = output
            .platform_output
            .commands
            .iter()
            .filter_map(|command| {
                if let egui::OutputCommand::CopyText(text) = command {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect();
        self.accessible_nodes = output
            .platform_output
            .accesskit_update
            .as_ref()
            .map(|update| update.nodes.clone())
            .unwrap_or_default();
        self.labels.clear();
        self.label_bounds.clear();
        pub(super) fn collect(
            shape: &egui::Shape,
            labels: &mut Vec<(String, egui::Pos2)>,
            bounds: &mut Vec<(String, egui::Rect)>,
        ) {
            match shape {
                egui::Shape::Text(text) => {
                    labels.push((
                        text.galley.text().to_owned(),
                        text.visual_bounding_rect().center(),
                    ));
                    bounds.push((text.galley.text().to_owned(), text.visual_bounding_rect()));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, labels, bounds);
                    }
                }
                _ => {}
            }
        }
        for shape in &output.shapes {
            collect(&shape.shape, &mut self.labels, &mut self.label_bounds);
        }
        self.shapes = output.shapes;
        output.textures_delta.clear();
    }

    pub(super) fn pointer(&mut self, pos: egui::Pos2, pressed: bool) {
        self.frame(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }

    pub(super) fn click(&mut self, pos: egui::Pos2) {
        self.pointer(pos, true);
        self.pointer(pos, false);
        self.frame(vec![]);
    }

    pub(super) fn input_pos(&self, id: &str) -> egui::Pos2 {
        self.ctx
            .read_response(egui::Id::new(id))
            .expect("input rendered")
            .rect
            .center()
    }

    pub(super) fn click_input(&mut self, id: &str) {
        self.click(self.input_pos(id));
    }

    pub(super) fn click_label(&mut self, label: &str) {
        let pos = self
            .labels
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("missing {label:?}: {:?}", self.labels))
            .1;
        self.click(pos);
    }

    pub(super) fn click_rendered_label(&mut self, label: &str) {
        for _ in 0..3 {
            self.frame(vec![]);
        }
        self.click_label(label);
    }

    pub(super) fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
        self.frame(vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }]);
        self.frame(vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        }]);
    }

    pub(super) fn open(&self, id: &str) -> bool {
        match id {
            "query_shortcut" => self.app.query_history_open,
            "package_filter" => self.app.package_popup_open,
            _ => unreachable!(),
        }
    }
}
