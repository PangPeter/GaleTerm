//! Load the Termius-inspired desktop theme (see `docs/UI.md`).

use std::rc::Rc;

use gpui_kit::component::theme::{Theme, ThemeMode, ThemeSet};
use gpui_kit::*;

const TERMIUS_THEME_JSON: &str = include_str!("../themes/gale-termius.json");

/// Apply **Termius Night** as the default desktop chrome + mono font.
pub fn apply_termius_theme(cx: &mut App) {
    let set: ThemeSet = match serde_json::from_str(TERMIUS_THEME_JSON) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(?err, "failed to parse gale-termius theme");
            return;
        }
    };

    let Some(dark) = set.themes.into_iter().find(|t| {
        matches!(t.mode, ThemeMode::Dark) || t.name.as_ref().contains("Termius")
    }) else {
        tracing::error!("gale-termius theme missing dark variant");
        return;
    };

    let dark = Rc::new(dark);
    Theme::update(cx, |theme| {
        theme.dark_theme = dark.clone();
        theme.mode = ThemeMode::Dark;
        theme.apply_config(&dark);
        theme.mono_font_family = "MesloLGS NF".into();
        theme.mono_font_size = px(13.);
    });
    tracing::info!("applied theme: Termius Night");
}
