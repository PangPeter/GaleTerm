//! Command palette (⌘K): hosts, open tabs, quick connect and app commands.

use gale_core::SessionProfile;
use gpui_kit::component::{
    input::{Escape, Input, InputState, MoveDown, MoveUp},
    label::Label,
    ActiveTheme, Icon, IconName, Sizable as _,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{
    parse_quick_connect, protocol_color, protocol_icon, session_target, GaleApp, Page,
    VaultSection,
};
use crate::i18n;

const MAX_HOSTS: usize = 8;

pub struct PaletteState {
    pub open: bool,
    pub input: Entity<InputState>,
    pub selected: usize,
    scroll: ScrollHandle,
}

impl PaletteState {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            open: false,
            input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(i18n::tr("app.palette_placeholder"))
            }),
            selected: 0,
            scroll: ScrollHandle::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Connect,
    Hosts,
    Tabs,
    Commands,
}

impl Group {
    fn label_key(self) -> &'static str {
        match self {
            Self::Connect => "app.palette_connect",
            Self::Hosts => "app.hosts",
            Self::Tabs => "app.palette_tabs",
            Self::Commands => "app.palette_commands",
        }
    }
}

#[derive(Clone, Copy)]
enum Command {
    NewLocal,
    Vault,
    Sftp,
    Settings,
    Refresh,
    CloseTab,
    NextTab,
    PrevTab,
    FullScreen,
    DesktopScaleFit,
    DesktopScaleNative,
    DesktopScaleStretch,
    CycleDesktopScale,
    ReconnectDesktop,
}

impl Command {
    const ALL: [Command; 14] = [
        Self::NewLocal,
        Self::Vault,
        Self::Sftp,
        Self::Settings,
        Self::Refresh,
        Self::CloseTab,
        Self::NextTab,
        Self::PrevTab,
        Self::FullScreen,
        Self::DesktopScaleFit,
        Self::DesktopScaleNative,
        Self::DesktopScaleStretch,
        Self::CycleDesktopScale,
        Self::ReconnectDesktop,
    ];

    fn label_key(self) -> &'static str {
        match self {
            Self::NewLocal => "app.cmd_new_local",
            Self::Vault => "app.cmd_vault",
            Self::Sftp => "app.cmd_sftp",
            Self::Settings => "app.cmd_settings",
            Self::Refresh => "app.cmd_refresh",
            Self::CloseTab => "app.cmd_close_tab",
            Self::NextTab => "app.cmd_next_tab",
            Self::PrevTab => "app.cmd_prev_tab",
            Self::FullScreen => "app.cmd_fullscreen",
            Self::DesktopScaleFit => "app.cmd_desktop_scale_fit",
            Self::DesktopScaleNative => "app.cmd_desktop_scale_native",
            Self::DesktopScaleStretch => "app.cmd_desktop_scale_stretch",
            Self::CycleDesktopScale => "app.cmd_desktop_scale_cycle",
            Self::ReconnectDesktop => "app.cmd_reconnect_desktop",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::NewLocal => IconName::SquareTerminal,
            Self::Vault => IconName::LayoutDashboard,
            Self::Sftp => IconName::Folder,
            Self::Settings => IconName::Settings,
            Self::Refresh | Self::ReconnectDesktop => IconName::RefreshCw,
            Self::CloseTab => IconName::Close,
            Self::NextTab => IconName::ChevronRight,
            Self::PrevTab => IconName::ChevronLeft,
            Self::FullScreen => IconName::Maximize,
            Self::DesktopScaleFit
            | Self::DesktopScaleNative
            | Self::DesktopScaleStretch
            | Self::CycleDesktopScale => IconName::Maximize,
        }
    }

    fn shortcut(self) -> Option<&'static str> {
        let mac = cfg!(target_os = "macos");
        Some(match self {
            Self::NewLocal => if mac { "⌘T" } else { "Ctrl+Shift+T" },
            Self::Settings => if mac { "⌘," } else { "Ctrl+Shift+," },
            Self::CloseTab => if mac { "⌘W" } else { "Ctrl+Shift+W" },
            Self::NextTab => "Ctrl+Tab",
            Self::PrevTab => "Ctrl+Shift+Tab",
            Self::FullScreen if mac => "⌃⌘F",
            Self::CycleDesktopScale => if mac { "⌘0" } else { "Ctrl+Shift+0" },
            Self::DesktopScaleFit => if mac { "⌘1" } else { "Ctrl+Shift+1" },
            Self::DesktopScaleNative => if mac { "⌘2" } else { "Ctrl+Shift+2" },
            Self::DesktopScaleStretch => if mac { "⌘3" } else { "Ctrl+Shift+3" },
            Self::ReconnectDesktop => if mac { "⌘R" } else { "Ctrl+Shift+R" },
            _ => return None,
        })
    }
}

#[derive(Clone)]
enum Target {
    Connect(Box<SessionProfile>),
    Tab(usize),
    Command(Command),
}

struct Item {
    group: Group,
    icon: IconName,
    /// Colored tile behind the icon (hosts / tabs); `None` = plain icon.
    tile: Option<Hsla>,
    title: SharedString,
    subtitle: Option<SharedString>,
    shortcut: Option<&'static str>,
    target: Target,
}

impl GaleApp {
    pub(super) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.open {
            self.close_palette(window, cx);
            return;
        }
        self.palette.open = true;
        self.palette.selected = 0;
        self.palette.input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.open = false;
        self.restore_focus(window, cx);
        cx.notify();
    }

    pub(super) fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.palette_items(cx);
        if let Some(item) = items.into_iter().nth(self.palette.selected) {
            self.palette_run(item.target, window, cx);
        }
    }

    fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.palette_items(cx).len();
        if len == 0 {
            return;
        }
        let next = (self.palette.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.palette.selected = next;
        self.palette.scroll.scroll_to_item(next);
        cx.notify();
    }

    fn palette_run(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.open = false;
        match target {
            Target::Connect(profile) => self.connect_profile(*profile, window, cx),
            Target::Tab(id) => self.select_tab(id, window, cx),
            Target::Command(cmd) => match cmd {
                Command::NewLocal => {
                    let _ = self.open_local(window, cx);
                }
                Command::Vault => self.show_page(Page::Vault, window, cx),
                Command::Sftp => self.show_page(Page::Sftp, window, cx),
                Command::Settings => {
                    self.section = VaultSection::Settings;
                    self.show_page(Page::Vault, window, cx);
                }
                Command::Refresh => {
                    self.refresh_sessions(cx);
                    self.restore_focus(window, cx);
                }
                Command::CloseTab => {
                    if let Some(id) = self.active_tab {
                        self.close_tab(id, window, cx);
                    }
                }
                Command::NextTab => self.cycle_tab(1, window, cx),
                Command::PrevTab => self.cycle_tab(-1, window, cx),
                Command::FullScreen => {
                    window.toggle_fullscreen();
                    self.restore_focus(window, cx);
                }
                Command::DesktopScaleFit => {
                    self.set_desktop_scale(gale_desktop_view::DesktopScaleMode::Fit, cx);
                    self.restore_focus(window, cx);
                }
                Command::DesktopScaleNative => {
                    self.set_desktop_scale(gale_desktop_view::DesktopScaleMode::Native, cx);
                    self.restore_focus(window, cx);
                }
                Command::DesktopScaleStretch => {
                    self.set_desktop_scale(gale_desktop_view::DesktopScaleMode::Stretch, cx);
                    self.restore_focus(window, cx);
                }
                Command::CycleDesktopScale => {
                    self.with_active_desktop_pane(cx, |pane, cx| pane.cycle_scale_mode(cx));
                    self.restore_focus(window, cx);
                }
                Command::ReconnectDesktop => {
                    self.reconnect_active_desktop(window, cx);
                }
            },
        }
        // Actions that leave focus alone still need it back from the palette input.
        if self.palette.input.read(cx).focus_handle(cx).is_focused(window) {
            self.restore_focus(window, cx);
        }
        cx.notify();
    }

    fn palette_items(&self, cx: &App) -> Vec<Item> {
        let raw = self.palette.input.read(cx).value().trim().to_string();
        let query = raw.to_lowercase();
        let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        let mut items = Vec::new();

        let looks_like_target =
            raw.contains('@') || raw.starts_with("ssh ") || raw.starts_with("telnet ");
        if looks_like_target {
            if let Some(profile) = parse_quick_connect(&raw) {
                items.push(Item {
                    group: Group::Connect,
                    icon: protocol_icon(profile.protocol),
                    tile: Some(protocol_color(profile.protocol)),
                    title: SharedString::from(session_target(&profile)),
                    subtitle: Some(SharedString::from(profile.protocol.as_str())),
                    shortcut: Some("↵"),
                    target: Target::Connect(Box::new(profile)),
                });
            }
        }

        items.extend(
            self.sessions
                .iter()
                .filter(|s| matches(&s.name) || matches(&session_target(s)))
                .take(MAX_HOSTS)
                .map(|s| Item {
                    group: Group::Hosts,
                    icon: protocol_icon(s.protocol),
                    tile: Some(protocol_color(s.protocol)),
                    title: SharedString::from(s.name.clone()),
                    subtitle: Some(SharedString::from(session_target(s))),
                    shortcut: None,
                    target: Target::Connect(Box::new(s.clone())),
                }),
        );

        items.extend(
            self.tabs
                .iter()
                .filter(|t| matches(&t.title))
                .map(|t| Item {
                    group: Group::Tabs,
                    icon: protocol_icon(t.protocol),
                    tile: Some(protocol_color(t.protocol)),
                    title: t.title.clone(),
                    subtitle: None,
                    shortcut: None,
                    target: Target::Tab(t.id),
                }),
        );

        items.extend(Command::ALL.into_iter().filter_map(|cmd| {
            let title = i18n::tr(cmd.label_key());
            matches(&title).then(|| Item {
                group: Group::Commands,
                icon: cmd.icon(),
                tile: None,
                title,
                subtitle: None,
                shortcut: cmd.shortcut(),
                target: Target::Command(cmd),
            })
        }));

        items
    }

    pub(super) fn render_palette(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.palette.open {
            return None;
        }
        let theme = cx.theme().clone();
        let items = self.palette_items(cx);
        let selected = self.palette.selected.min(items.len().saturating_sub(1));

        let mut prev_group = None;
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(ix, item)| {
                let header = (prev_group != Some(item.group)).then(|| {
                    div()
                        .px_3()
                        .pt_3()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(i18n::tr(item.group.label_key()))
                });
                prev_group = Some(item.group);
                let is_selected = ix == selected;
                let target = item.target.clone();

                let icon = match item.tile {
                    Some(color) => div()
                        .w(px(24.))
                        .h(px(24.))
                        .rounded(px(6.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(color)
                        .child(Icon::new(item.icon).xsmall().text_color(white())),
                    None => div()
                        .w(px(24.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(item.icon).small().text_color(theme.muted_foreground)),
                };

                div()
                    .id(("palette-row", ix))
                    .flex()
                    .flex_col()
                    .children(header)
                    .child(
                        div()
                            .mx_1()
                            .h(px(40.))
                            .px_2()
                            .rounded(px(8.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .when(is_selected, |s| s.bg(theme.accent))
                            .when(!is_selected, |s| s.hover(|s| s.bg(theme.list_hover)))
                            .child(icon)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_row()
                                    .items_baseline()
                                    .gap_2()
                                    .overflow_hidden()
                                    .child(Label::new(item.title).text_sm())
                                    .children(item.subtitle.map(|sub| {
                                        Label::new(sub)
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                    })),
                            )
                            .children(item.shortcut.map(|keys| {
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(4.))
                                    .bg(theme.secondary)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(keys)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.palette_run(target.clone(), window, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        let empty = rows.is_empty();
        let card = div()
            .w(px(600.))
            .max_h(px(460.))
            .rounded(px(14.))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Input::new(&self.palette.input)
                            .large()
                            .appearance(false)
                            .prefix(Icon::new(IconName::Search).small()),
                    ),
            )
            .child(
                div()
                    .id("palette-list")
                    .flex_1()
                    .min_h_0()
                    .pb_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.palette.scroll)
                    .when(empty, |s| {
                        s.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(i18n::tr("app.no_match")),
                        )
                    })
                    .children(rows),
            );

        Some(
            div()
                .id("palette-overlay")
                .key_context("CommandPalette")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .bg(hsla(0., 0., 0., 0.35))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(88.))
                .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                    this.palette_move(-1, cx);
                    cx.stop_propagation();
                }))
                .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                    this.palette_move(1, cx);
                    cx.stop_propagation();
                }))
                .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                    this.close_palette(window, cx);
                    cx.stop_propagation();
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                )
                .child(card)
                .into_any_element(),
        )
    }
}
