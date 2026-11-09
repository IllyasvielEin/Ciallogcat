use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::i18n::Language;
use crate::model::{Level, LogBuffer, SavedFilter};

#[cfg(not(target_os = "windows"))]
const APP_DIR: &str = "ciallogcat";
const CONFIG_FILE: &str = "config.json";

/// Window theme. Absent in configs written before the settings panel; those
/// keep their legacy `dark` choice (see [`AppConfig::theme_choice`]).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    System,
    Dark,
    Light,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub package: String,
    pub tag_include: String,
    pub tag_exclude: String,
    pub min_level: Level,
    pub query: String,
    pub use_regex: bool,
    pub case_sensitive: bool,
    pub dark: bool,
    pub theme: Option<ThemeChoice>,
    /// `None` only for configs written before the setting existed.
    pub language: Option<Language>,
    pub row_height: f32,
    pub show_details: bool,
    pub saved_filters: Vec<SavedFilter>,
    pub filter_history: Vec<String>,
    pub memory_limit_mib: usize,
    pub buffers: Vec<LogBuffer>,
    pub capture_scope: ciallogcat::capture_scope::CaptureScope,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            package: String::new(),
            tag_include: String::new(),
            tag_exclude: String::new(),
            min_level: Level::Verbose,
            query: String::new(),
            use_regex: false,
            case_sensitive: false,
            dark: true,
            theme: None,
            language: None,
            row_height: 25.0,
            show_details: true,
            saved_filters: Vec::new(),
            filter_history: Vec::new(),
            memory_limit_mib: 300,
            buffers: LogBuffer::defaults(),
            capture_scope: ciallogcat::capture_scope::CaptureScope::default(),
        }
    }
}

impl AppConfig {
    pub fn theme_choice(&self) -> ThemeChoice {
        self.theme.unwrap_or(if self.dark {
            ThemeChoice::Dark
        } else {
            ThemeChoice::Light
        })
    }

    pub fn language(&self) -> Language {
        self.language.unwrap_or_else(Language::legacy)
    }

    pub fn load() -> Self {
        Self::load_from(config_path())
    }

    fn load_from(path: Option<PathBuf>) -> Self {
        // A first run (no settings file yet) starts in English.
        let first_run = || Self {
            language: Some(Language::En),
            ..Self::default()
        };
        let Some(path) = path else {
            return first_run();
        };
        let contents = fs::read_to_string(&path);
        let Ok(contents) = contents else {
            return first_run();
        };
        let mut config: Self = serde_json::from_str(&contents).unwrap_or_default();
        // An existing file without the field predates the language setting.
        config.language.get_or_insert_with(Language::legacy);
        config.row_height = config.row_height.clamp(21.0, 34.0);
        config.memory_limit_mib = config.memory_limit_mib.clamp(16, 4096);
        LogBuffer::normalize(&mut config.buffers);
        config
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no user configuration directory")
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(path, contents)
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("CIALLOGCAT_CONFIG_DIR") {
        return Some(PathBuf::from(path).join(CONFIG_FILE));
    }

    #[cfg(target_os = "windows")]
    {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("Ciallogcat").join(CONFIG_FILE))
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(path).join(APP_DIR).join(CONFIG_FILE));
        }
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".config").join(APP_DIR).join(CONFIG_FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_keeps_saved_filters() {
        let mut config = AppConfig {
            filter_history: vec!["timeout|失败".to_owned(), " error ".to_owned()],
            memory_limit_mib: 128,
            buffers: vec![LogBuffer::Events, LogBuffer::Crash],
            ..Default::default()
        };
        config.saved_filters.push(SavedFilter {
            name: "Network".to_owned(),
            package: "com.example.app".to_owned(),
            tag_include: "Network".to_owned(),
            tag_exclude: "Noise".to_owned(),
            min_level: Level::Debug,
            query: "timeout|failed".to_owned(),
            regex: true,
            case_sensitive: false,
        });

        let encoded = serde_json::to_string(&config).unwrap();
        let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.saved_filters, config.saved_filters);
        assert_eq!(decoded.filter_history, config.filter_history);
        assert_eq!(decoded.memory_limit_mib, 128);
        assert_eq!(decoded.buffers, config.buffers);
    }

    #[test]
    fn partial_preferences_use_defaults_for_unspecified_settings() {
        let config: AppConfig = serde_json::from_str(r#"{"dark":false}"#).unwrap();
        assert_eq!(config.memory_limit_mib, 300);
        assert!(config.filter_history.is_empty());
        assert_eq!(config.buffers, LogBuffer::defaults());
        assert!(config.tag_include.is_empty() && config.tag_exclude.is_empty());
        let filter: SavedFilter = serde_json::from_str(r#"{"name":"partial","package":"","min_level":"Info","query":"","regex":false,"case_sensitive":false}"#).unwrap();
        assert!(filter.tag_include.is_empty() && filter.tag_exclude.is_empty());
    }

    #[test]
    fn first_run_is_english_and_upgraded_configs_keep_chinese() {
        let dir = std::env::temp_dir().join(format!("ciallogcat-lang-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE);
        let _ = fs::remove_file(&path);
        assert_eq!(
            AppConfig::load_from(Some(path.clone())).language(),
            Language::En
        );
        assert_eq!(AppConfig::load_from(None).language(), Language::En);
        fs::write(&path, r#"{"dark":false}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(Some(path.clone())).language(),
            Language::ZhCn
        );
        fs::write(&path, r#"{"language":"en"}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(Some(path.clone())).language(),
            Language::En
        );
        fs::write(&path, r#"{"language":"zh-CN"}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(Some(path.clone())).language(),
            Language::ZhCn
        );
        let saved = AppConfig {
            language: Some(Language::En),
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains(r#""language":"en""#), "{json}");
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/support/config_integration.rs"]
mod integration_tests;
