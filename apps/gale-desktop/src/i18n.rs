//! Locale preference: follow system language, or pin en / zh-CN.

use std::fs;
use std::path::PathBuf;

use gpui_kit::component::{locale, set_locale};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

/// Supported UI locales (rust-i18n / gpui-component codes).
pub const LOCALE_EN: &str = "en";
pub const LOCALE_ZH_CN: &str = "zh-CN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LocalePreference {
    /// Detect from OS (`LANG` / `sys-locale`); Chinese → zh-CN, else en.
    #[default]
    Auto,
    En,
    ZhCn,
}

impl LocalePreference {
    pub fn as_storage_key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::En => "en",
            Self::ZhCn => "zh-CN",
        }
    }

    #[allow(dead_code)]
    pub fn from_storage_key(s: &str) -> Self {
        match s {
            "en" | "en-US" | "en_US" => Self::En,
            "zh-CN" | "zh_CN" | "zh" | "zh-Hans" => Self::ZhCn,
            _ => Self::Auto,
        }
    }

    /// Resolved rust-i18n locale code.
    pub fn resolve(self) -> &'static str {
        match self {
            Self::En => LOCALE_EN,
            Self::ZhCn => LOCALE_ZH_CN,
            Self::Auto => detect_system_locale(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiSettings {
    #[serde(default)]
    pub locale: LocalePreference,
}

impl UiSettings {
    pub fn path() -> anyhow::Result<PathBuf> {
        let dir = dirs::data_dir()
            .or_else(dirs::config_dir)
            .ok_or_else(|| anyhow::anyhow!("no data/config dir"))?
            .join("gale-term");
        Ok(dir.join("ui-settings.json"))
    }

    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Map OS locale tags to a supported app locale.
pub fn detect_system_locale() -> &'static str {
    let raw = sys_locale::get_locale().unwrap_or_default();
    let normalized = raw.replace('_', "-").to_lowercase();
    if normalized.starts_with("zh") {
        // zh, zh-CN, zh-Hans, zh-TW → 目前仅简体；繁体暂映射到简体文案
        LOCALE_ZH_CN
    } else {
        LOCALE_EN
    }
}

/// Apply preference to rust-i18n / gpui-component and refresh windows.
pub fn apply_locale(pref: LocalePreference, cx: &mut App) {
    let code = pref.resolve();
    set_locale(code);
    tracing::info!(locale = code, preference = ?pref, "UI locale applied");
    cx.refresh_windows();
}

#[allow(dead_code)]
pub fn current_locale() -> String {
    locale().to_string()
}

/// Translate an application key (looked up at runtime in `locales/ui.yml`).
pub fn tr(key: &str) -> SharedString {
    SharedString::from(rust_i18n::t!(key).to_string())
}

pub fn tr_count(key: &str, count: usize) -> SharedString {
    let s = match key {
        "app.status_sessions" => rust_i18n::t!("app.status_sessions", count = count),
        "files.items" => rust_i18n::t!("files.items", count = count),
        "files.delete_title" => rust_i18n::t!("files.delete_title", count = count),
        "files.overwrite_title" => rust_i18n::t!("files.overwrite_title", count = count),
        "files.chmod_title" => rust_i18n::t!("files.chmod_title", count = count),
        other => std::borrow::Cow::Owned(format!("{other}:{count}")),
    };
    SharedString::from(s.to_string())
}

pub fn tr_selected(selected: usize, total: usize) -> SharedString {
    SharedString::from(
        rust_i18n::t!("files.selected", selected = selected, total = total).to_string(),
    )
}

pub fn tr_error(key: &str, error: &str) -> SharedString {
    let s = match key {
        "app.status_local_failed" => rust_i18n::t!("app.status_local_failed", error = error),
        "app.connect_failed" => rust_i18n::t!("app.connect_failed", error = error),
        other => std::borrow::Cow::Owned(format!("{other}: {error}")),
    };
    SharedString::from(s.to_string())
}
