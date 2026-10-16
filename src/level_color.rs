//! Desktop presentation for headless log levels.

use ciallogcat::model::Level;
use eframe::egui::Color32;

pub trait LevelColor {
    fn color(self, dark: bool) -> Color32;
}

impl LevelColor for Level {
    fn color(self, dark: bool) -> Color32 {
        match (self, dark) {
            (Self::Verbose, true) => Color32::from_rgb(142, 151, 166),
            (Self::Debug, true) => Color32::from_rgb(112, 161, 255),
            (Self::Info, true) => Color32::from_rgb(101, 203, 152),
            (Self::Warn, true) => Color32::from_rgb(239, 190, 82),
            (Self::Error | Self::Fatal, true) => Color32::from_rgb(255, 111, 118),
            (Self::Verbose, false) => Color32::from_rgb(91, 100, 114),
            (Self::Debug, false) => Color32::from_rgb(45, 103, 196),
            (Self::Info, false) => Color32::from_rgb(28, 130, 79),
            (Self::Warn, false) => Color32::from_rgb(166, 102, 0),
            (Self::Error | Self::Fatal, false) => Color32::from_rgb(194, 48, 55),
        }
    }
}
