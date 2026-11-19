//! App actions, key bindings and the native menu bar.

use gpui_kit::component::input::{Copy, Cut, Paste, SelectAll};
use gpui_kit::*;

use crate::i18n::tr;

actions!(
    gale,
    [
        Quit,
        Hide,
        Minimize,
        ToggleFullScreen,
        OpenSettings,
        NewLocalTerminal,
        QuickConnect,
        CloseTab,
        NextTab,
        PrevTab,
        ToggleCommandPalette,
        ShowVault,
        ShowSftp,
        RefreshHosts,
    ]
);

/// Key context of the terminal stage (bindings that only apply there).
pub const TERMINAL_CONTEXT: &str = "GaleTerminal";

/// Primary chord: `cmd-…` on macOS, `ctrl-shift-…` elsewhere so plain
/// `ctrl-…` keeps reaching the shell.
#[cfg(target_os = "macos")]
macro_rules! primary {
    ($key:literal) => {
        concat!("cmd-", $key)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! primary {
    ($key:literal) => {
        concat!("ctrl-shift-", $key)
    };
}

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(primary!("q"), Quit, None),
        KeyBinding::new(primary!(","), OpenSettings, None),
        KeyBinding::new(primary!("t"), NewLocalTerminal, None),
        KeyBinding::new(primary!("w"), CloseTab, None),
        KeyBinding::new(primary!("k"), ToggleCommandPalette, None),
        KeyBinding::new(primary!("p"), ToggleCommandPalette, None),
        KeyBinding::new(primary!("n"), QuickConnect, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new(primary!("v"), Paste, Some(TERMINAL_CONTEXT)),
        KeyBinding::new(primary!("c"), Copy, Some(TERMINAL_CONTEXT)),
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("cmd-m", Minimize, None),
            KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
            KeyBinding::new("cmd-}", NextTab, None),
            KeyBinding::new("cmd-{", PrevTab, None),
        ]);
    }

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    install_menus(cx);
}

/// (Re)build the menu bar in the current UI language.
pub fn install_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("GaleTerm").items([
            MenuItem::action(tr("app.cmd_settings"), OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(tr("app.menu_services"), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(tr("app.cmd_hide"), Hide),
            MenuItem::action(tr("app.cmd_quit"), Quit),
        ]),
        Menu::new(tr("app.menu_file")).items([
            MenuItem::action(tr("app.cmd_new_local"), NewLocalTerminal),
            MenuItem::action(tr("app.cmd_quick_connect"), QuickConnect),
            MenuItem::separator(),
            MenuItem::action(tr("app.cmd_close_tab"), CloseTab),
        ]),
        Menu::new(tr("app.menu_edit")).items([
            MenuItem::os_action(tr("app.cmd_cut"), Cut, OsAction::Cut),
            MenuItem::os_action(tr("app.cmd_copy"), Copy, OsAction::Copy),
            MenuItem::os_action(tr("app.cmd_paste"), Paste, OsAction::Paste),
            MenuItem::os_action(tr("app.cmd_select_all"), SelectAll, OsAction::SelectAll),
        ]),
        Menu::new(tr("app.menu_view")).items([
            MenuItem::action(tr("app.cmd_palette"), ToggleCommandPalette),
            MenuItem::separator(),
            MenuItem::action(tr("app.cmd_vault"), ShowVault),
            MenuItem::action(tr("app.cmd_sftp"), ShowSftp),
            MenuItem::action(tr("app.cmd_refresh"), RefreshHosts),
            MenuItem::separator(),
            MenuItem::action(tr("app.cmd_fullscreen"), ToggleFullScreen),
        ]),
        Menu::new(tr("app.menu_window")).items([
            MenuItem::action(tr("app.cmd_minimize"), Minimize),
            MenuItem::separator(),
            MenuItem::action(tr("app.cmd_next_tab"), NextTab),
            MenuItem::action(tr("app.cmd_prev_tab"), PrevTab),
        ]),
    ]);
}
