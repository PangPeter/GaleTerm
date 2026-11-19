//! Embed and register MesloLGS NF / MesloLGS Nerd Font for the terminal.

use std::borrow::Cow;

use gpui_kit::component::theme::Theme;
use gpui_kit::*;

/// Preferred terminal monospace family (powerlevel10k MesloLGS NF).
pub const TERM_FONT_FAMILY: &str = "MesloLGS NF";

/// Alternate family name from nerd-fonts packaging (also bundled).
pub const TERM_FONT_FAMILY_ALT: &str = "MesloLGS Nerd Font";

macro_rules! font_bytes {
    ($file:literal) => {
        include_bytes!(concat!("../assets/fonts/", $file)).as_slice()
    };
}

/// Register bundled faces and set the theme mono font. Call after `init(cx)`,
/// before opening windows that measure text.
pub fn register_bundled_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        // MesloLGS NF (powerlevel10k)
        Cow::Borrowed(font_bytes!("MesloLGSNF-Regular.ttf")),
        Cow::Borrowed(font_bytes!("MesloLGSNF-Bold.ttf")),
        Cow::Borrowed(font_bytes!("MesloLGSNF-Italic.ttf")),
        Cow::Borrowed(font_bytes!("MesloLGSNF-BoldItalic.ttf")),
        // MesloLGS Nerd Font (nerd-fonts)
        Cow::Borrowed(font_bytes!("MesloLGSNerdFont-Regular.ttf")),
        Cow::Borrowed(font_bytes!("MesloLGSNerdFont-Bold.ttf")),
        Cow::Borrowed(font_bytes!("MesloLGSNerdFont-Italic.ttf")),
        Cow::Borrowed(font_bytes!("MesloLGSNerdFont-BoldItalic.ttf")),
    ];

    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::error!(?err, "failed to register bundled Meslo fonts");
        return;
    }

    let names = cx.text_system().all_font_names();
    let has_nf = names.iter().any(|n| n == TERM_FONT_FAMILY);
    let has_nerd = names.iter().any(|n| n == TERM_FONT_FAMILY_ALT);
    tracing::info!(
        meslo_lgs_nf = has_nf,
        meslo_lgs_nerd_font = has_nerd,
        "bundled terminal fonts registered"
    );

    Theme::update(cx, |theme| {
        theme.mono_font_family = TERM_FONT_FAMILY.into();
        theme.mono_font_size = px(13.);
    });
}
