use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{Level, LogBuffer, SavedFilter};

#[cfg(not(target_os = "windows"))]
const APP_DIR: &str = "ciallogcat";
const CONFIG_FILE: &str = "config.json";

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
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let contents = fs::read_to_string(&path);
        let Ok(contents) = contents else {
            return Self::default();
        };
        let mut config: Self = serde_json::from_str(&contents).unwrap_or_default();
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
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/support/config_integration.rs"]
mod integration_tests;
