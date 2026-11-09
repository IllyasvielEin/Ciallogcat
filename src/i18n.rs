//! Desktop UI language selection (English / Simplified Chinese).
//!
//! Translations live in `locales/app.yml`, keyed by the original Simplified
//! Chinese text (gettext style): `tr!("开始采集")` yields "Start capture" in
//! English. An unknown key is returned unchanged, so passing through dynamic
//! text such as an ADB error is harmless.
//!
//! The language is tracked per thread. The UI thread sets it from the config;
//! background workers that format user-facing text call [`set`] with the
//! language captured when they were spawned. Parallel tests therefore never
//! observe another test's language.
use std::cell::Cell;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Language {
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::En, Self::ZhCn];

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
        }
    }

    /// Each language names itself, so it can be found from either UI.
    pub fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::ZhCn => "简体中文",
        }
    }

    /// Configs written before the language setting existed belong to users of
    /// the Chinese-only UI; they keep Chinese after upgrading.
    pub fn legacy() -> Self {
        Self::ZhCn
    }
}

// rust-i18n reads the catalog inside a proc macro, which cargo does not track.
// Including it here records it in dep-info, so editing the YAML recompiles
// the crate (and re-runs `i18n!`).
const _CATALOG_DEPENDENCY: &str = include_str!("../locales/app.yml");

thread_local! {
    static CURRENT: Cell<Language> = const { Cell::new(Language::En) };
}

pub fn set(language: Language) {
    CURRENT.with(|current| current.set(language));
}

pub fn current() -> Language {
    CURRENT.with(Cell::get)
}

/// Translate a key (the Simplified Chinese source text) into the current
/// thread's language. Named arguments fill `%{name}` placeholders.
macro_rules! tr {
    ($key:expr) => {
        ::rust_i18n::t!($key, locale = $crate::i18n::current().code()).into_owned()
    };
    ($key:expr, $($args:tt)+) => {
        ::rust_i18n::t!($key, locale = $crate::i18n::current().code(), $($args)+).into_owned()
    };
}
pub(crate) use tr;

/// Core-library messages use their Chinese text as the key; a few carry a
/// dynamic suffix (for example an ADB error) after a fixed prefix. Unknown
/// text, such as raw tool output, is returned unchanged.
pub fn localize_message(message: &str) -> String {
    const PREFIXES: [&str; 1] = ["自动重连已尝试 3 次："];
    for prefix in PREFIXES {
        if let Some(rest) = message.strip_prefix(prefix) {
            return format!("{}{rest}", tr!(prefix));
        }
    }
    tr!(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every module whose Chinese literals are shown through `tr!`, either
    /// directly or as table/label keys translated at the call site.
    const SOURCES: [(&str, &str); 11] = [
        ("main.rs", include_str!("main.rs")),
        ("file_ui.rs", include_str!("file_ui.rs")),
        ("diagnostics_ui.rs", include_str!("diagnostics_ui.rs")),
        ("settings_ui.rs", include_str!("settings_ui.rs")),
        ("capture_ui.rs", include_str!("capture_ui.rs")),
        ("reading_ui.rs", include_str!("reading_ui.rs")),
        ("copy.rs", include_str!("copy.rs")),
        ("i18n.rs", include_str!("i18n.rs")),
        ("process_watch.rs", include_str!("process_watch.rs")),
        ("incident.rs", include_str!("incident.rs")),
        (
            "capture_controller.rs",
            include_str!("capture_controller.rs"),
        ),
    ];
    const CATALOG: &str = include_str!("../locales/app.yml");
    /// Literals that are deliberately not translated.
    const UNTRANSLATED: [&str; 1] = ["简体中文"];

    fn production(source: &str) -> &str {
        let test_module = regex::Regex::new(r"#\[cfg\(test\)\]\s*\n\s*mod \w+ \{").unwrap();
        test_module
            .find(source)
            .map_or(source, |found| &source[..found.start()])
    }

    fn source_keys() -> BTreeSet<String> {
        let literal = regex::Regex::new(r#""((?:[^"\\\n]|\\.)*)""#).unwrap();
        let mut keys = BTreeSet::new();
        for (_, source) in SOURCES {
            for line in production(source).lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for found in literal.captures_iter(line) {
                    let raw = &found[1];
                    let key = raw
                        .replace("\\n", "\n")
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\");
                    // Library format strings are not keys; their messages
                    // reach the UI through `localize_message`.
                    let formatted = key.contains('{') && !key.contains("%{");
                    if key.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
                        && !formatted
                        && !UNTRANSLATED.contains(&key.as_str())
                    {
                        keys.insert(key);
                    }
                }
            }
        }
        keys
    }

    fn catalog_keys() -> BTreeSet<String> {
        CATALOG
            .lines()
            .filter(|line| line.starts_with('"'))
            .map(|line| serde_json::from_str(line.trim_end_matches(':')).unwrap())
            .collect()
    }

    fn placeholders(text: &str) -> BTreeSet<String> {
        regex::Regex::new(r"%\{(\w+)\}")
            .unwrap()
            .captures_iter(text)
            .map(|found| found[1].to_owned())
            .collect()
    }

    #[test]
    fn every_ui_string_has_both_translations_and_matching_placeholders() {
        let used = source_keys();
        assert!(used.len() > 250, "source scan found only {}", used.len());
        for key in &used {
            let english = crate::_rust_i18n_try_translate("en", key)
                .unwrap_or_else(|| panic!("missing en: {key:?}"));
            let chinese = crate::_rust_i18n_try_translate("zh-CN", key)
                .unwrap_or_else(|| panic!("missing zh-CN: {key:?}"));
            assert_eq!(chinese, key.as_str(), "zh-CN must equal its key");
            assert!(
                !english
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
                    || key == "语言 / Language",
                "untranslated en: {key:?} -> {english:?}"
            );
            assert_eq!(placeholders(&english), placeholders(key), "{key:?}");
        }
        let orphans: Vec<_> = catalog_keys().difference(&used).cloned().collect();
        assert!(orphans.is_empty(), "unused catalog entries: {orphans:?}");
    }

    #[test]
    fn language_is_per_thread_and_unknown_text_passes_through() {
        set(Language::En);
        assert_eq!(tr!("开始采集"), "Start capture");
        assert_eq!(tr!("%{count} 条匹配", count = 3), "3 matches");
        assert_eq!(tr!("adb: device offline"), "adb: device offline");
        assert_eq!(
            localize_message("自动重连已尝试 3 次：adb: device offline"),
            "Automatic reconnect tried 3 times: adb: device offline"
        );
        std::thread::spawn(|| {
            set(Language::ZhCn);
            assert_eq!(tr!("开始采集"), "开始采集");
        })
        .join()
        .unwrap();
        assert_eq!(current(), Language::En);
    }
}
