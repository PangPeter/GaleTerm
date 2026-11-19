//! GaleTerm desktop — Phase 3 (Termius-style: top nav / Vault / terminal tabs).

mod app;
mod file_icons;
mod files;
mod fonts;
mod i18n;
mod local_session;
mod menus;
mod remote_desktop;
mod remote_session;
mod theme;

use gpui_kit::*;

rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    application()
        .with_assets(file_icons::GaleAssets)
        .run(|cx| {
            {
                use gpui_kit::component as gpui_component;
                rust_i18n::extend!(gpui_component);
            }
            init(cx);

            let settings = i18n::UiSettings::load();
            i18n::apply_locale(settings.locale, cx);
            menus::init(cx);
            fonts::register_bundled_fonts(cx);
            theme::apply_termius_theme(cx);

            let title = i18n::tr("app.window_title");
            let base = component::TitleBar::window_options();
            open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1200.), px(780.)),
                        cx,
                    ))),
                    titlebar: Some(TitlebarOptions {
                        title: Some(title),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(14.), px(14.))),
                    }),
                    window_min_size: Some(size(px(760.), px(480.))),
                    ..base
                },
                cx,
                |window, cx| cx.new(|cx| app::GaleApp::new(window, cx)),
            )
            .expect("open GaleTerm window");
        });
}
