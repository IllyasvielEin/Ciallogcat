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
    pub min_level: Level,
    pub query: String,
    pub use_regex: bool,
    pub case_sensitive: bool,
    pub dark: bool,
    pub row_height: f32,
    pub show_details: bool,
    pub saved_filters: Vec<SavedFilter>,
    pub memory_limit_mib: usize,
    pub buffers: Vec<LogBuffer>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            package: String::new(),
            min_level: Level::Verbose,
            query: String::new(),
            use_regex: false,
            case_sensitive: false,
            dark: true,
            row_height: 25.0,
            show_details: true,
            saved_filters: Vec::new(),
            memory_limit_mib: 300,
            buffers: LogBuffer::defaults(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let contents = fs::read_to_string(&path).or_else(|error| {
            // Import old default settings without overwriting or removing them.
            // Explicit configuration directories must remain isolated.
            if error.kind() == io::ErrorKind::NotFound
                && env::var_os("CIALLOGCAT_CONFIG_DIR").is_none()
                && let Some(base) = path.parent().and_then(|parent| parent.parent())
            {
                let legacy_dir = if cfg!(target_os = "windows") {
                    "Ciallocat"
                } else {
                    "ciallocat"
                };
                return fs::read_to_string(base.join(legacy_dir).join(CONFIG_FILE));
            }
            Err(error)
        });
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
            memory_limit_mib: 128,
            buffers: vec![LogBuffer::Events, LogBuffer::Crash],
            ..Default::default()
        };
        config.saved_filters.push(SavedFilter {
            name: "Network".to_owned(),
            package: "com.example.app".to_owned(),
            min_level: Level::Debug,
            query: "timeout|failed".to_owned(),
            regex: true,
            case_sensitive: false,
        });

        let encoded = serde_json::to_string(&config).unwrap();
        let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.saved_filters, config.saved_filters);
        assert_eq!(decoded.memory_limit_mib, 128);
        assert_eq!(decoded.buffers, config.buffers);
    }

    #[test]
    fn old_configuration_gets_memory_and_buffer_defaults() {
        let config: AppConfig = serde_json::from_str(r#"{"dark":false}"#).unwrap();
        assert_eq!(config.memory_limit_mib, 300);
        assert_eq!(config.buffers, LogBuffer::defaults());
    }
}
