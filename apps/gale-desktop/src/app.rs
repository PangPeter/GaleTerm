//! Main workspace shell — Termius X layout: immersive title bar with Vault / SFTP /
//! connection tabs, Vault with a left nav, full-bleed terminals.
//! Layout & tone: `docs/UI.md`.

use std::collections::VecDeque;

use gale_core::vault::{SavedCredential, Vault};
use gale_core::{AuthMethod, FtpSecurity, Protocol, SessionProfile, SessionStore, SshAuth};
use gale_ftp::CertDecision;
use gale_ssh::HostKeyDecision;
use gale_term_view::TerminalPane;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Copy, Input, InputEvent, InputState, Paste},
    label::Label,
    ActiveTheme, Icon, IconName, Sizable as _, TitleBar,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::files::FileManager;
use crate::i18n::{self, LocalePreference, UiSettings};
use crate::local_session::LocalTerminalTab;
use crate::menus;

mod palette;
mod keychain;
use crate::remote_session::{
    GuiPrompts, PromptRequest, RemoteTarget, RemoteTerminalTab, SecretAnswer,
};

const TITLE_BAR_H: f32 = 44.;
const SIDEBAR_W: f32 = 220.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Vault,
    Sftp,
    Terminal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VaultSection {
    Hosts,
    Keychain,
    PortForwarding,
    Snippets,
    KnownHosts,
    Settings,
}

impl VaultSection {
    fn label_key(self) -> &'static str {
        match self {
            Self::Hosts => "app.hosts",
            Self::Keychain => "app.keychain",
            Self::PortForwarding => "app.port_forwarding",
            Self::Snippets => "app.snippets",
            Self::KnownHosts => "app.known_hosts",
            Self::Settings => "app.settings",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Hosts => IconName::HardDrive,
            Self::Keychain => IconName::User,
            Self::PortForwarding => IconName::Network,
            Self::Snippets => IconName::FileText,
            Self::KnownHosts => IconName::Globe,
            Self::Settings => IconName::Settings,
        }
    }
}

enum TabBackend {
    Local(LocalTerminalTab),
    Remote(RemoteTerminalTab),
    Desktop(#[allow(dead_code)] crate::remote_desktop::RemoteDesktopTab),
}

impl TabBackend {
    fn write_input(&self, data: &[u8]) {
        match self {
            Self::Local(tab) => tab.write_input(data),
            Self::Remote(tab) => tab.write_input(data),
            Self::Desktop(_) => {}
        }
    }

    fn is_desktop(&self) -> bool {
        matches!(self, Self::Desktop(_))
    }
}

enum TabContent {
    Terminal(Entity<TerminalPane>),
    Desktop(Entity<gale_desktop_view::DesktopPane>),
}

struct OpenTab {
    id: usize,
    title: SharedString,
    protocol: Protocol,
    content: TabContent,
    backend: TabBackend,
    /// Remote side has gone away; the pane stays open for reading.
    closed: bool,
}

impl OpenTab {
    fn terminal(&self) -> Option<&Entity<TerminalPane>> {
        match &self.content {
            TabContent::Terminal(pane) => Some(pane),
            TabContent::Desktop(_) => None,
        }
    }
}

pub struct GaleApp {
    sessions: Vec<SessionProfile>,
    store_error: Option<SharedString>,
    tabs: Vec<OpenTab>,
    active_tab: Option<usize>,
    next_tab_id: usize,
    page: Page,
    section: VaultSection,
    search: Entity<InputState>,
    root_focus: FocusHandle,
    term_focus: FocusHandle,
    palette: palette::PaletteState,
    notice: Option<SharedString>,
    locale_pref: LocalePreference,
    prompts: GuiPrompts,
    /// Pending questions from network tasks; the front one is on screen.
    prompt_queue: VecDeque<PromptRequest>,
    secret_input: Entity<InputState>,
    /// "Remember in keychain" checkbox of the password prompt.
    remember_secret: bool,
    vault: Option<Vault>,
    /// Keychain page rows, reloaded when the page opens.
    credentials: Vec<SavedCredential>,
    files: Entity<FileManager>,
    _subscriptions: Vec<Subscription>,
}

impl GaleApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = UiSettings::load();
        let (sessions, store_error) = match SessionStore::open_default() {
            Ok(store) => (store.list().to_vec(), None),
            Err(err) => (Vec::new(), Some(SharedString::from(err.to_string()))),
        };

        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(i18n::tr("app.search_hosts"))
        });
        let secret_input = cx.new(|cx| InputState::new(window, cx).masked(true));
        let palette = palette::PaletteState::new(window, cx);
        let subscriptions = vec![
            cx.subscribe_in(
                &palette.input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.palette.selected = 0;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.palette_confirm(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &search,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => this.connect_from_search(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &secret_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.answer_secret(true, window, cx);
                    }
                },
            ),
        ];

        let (prompt_tx, prompt_rx) = async_channel::unbounded::<PromptRequest>();
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(req) = prompt_rx.recv().await {
                if this
                    .update_in(cx, |this, window, cx| this.push_prompt(req, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let root_focus = cx.focus_handle();
        window.focus(&root_focus, cx);

        let vault = Vault::open_default()
            .inspect_err(|err| tracing::warn!(%err, "credential vault unavailable"))
            .ok();
        let prompts = GuiPrompts::new(prompt_tx, vault.clone());
        let files = {
            let prompts = prompts.clone();
            let hosts = sessions.clone();
            cx.new(|cx| FileManager::new(prompts, hosts, window, cx))
        };

        Self {
            sessions,
            store_error,
            tabs: Vec::new(),
            active_tab: None,
            next_tab_id: 1,
            page: Page::Vault,
            section: VaultSection::Hosts,
            search,
            root_focus,
            term_focus: cx.focus_handle(),
            palette,
            notice: None,
            locale_pref: settings.locale,
            prompts,
            prompt_queue: VecDeque::new(),
            secret_input,
            remember_secret: false,
            vault,
            credentials: Vec::new(),
            files,
            _subscriptions: subscriptions,
        }
    }

    fn set_locale_pref(&mut self, pref: LocalePreference, window: &mut Window, cx: &mut Context<Self>) {
        self.locale_pref = pref;
        let mut settings = UiSettings::load();
        settings.locale = pref;
        if let Err(err) = settings.save() {
            tracing::warn!(?err, "failed to save UI locale preference");
        }
        i18n::apply_locale(pref, cx);
        menus::install_menus(cx);
        let local_title = i18n::tr("app.local_tab");
        for tab in &mut self.tabs {
            if matches!(tab.backend, TabBackend::Local(_)) {
                tab.title = local_title.clone();
            }
        }
        let placeholder = i18n::tr("app.search_hosts");
        self.search
            .update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
        let placeholder = i18n::tr("app.palette_placeholder");
        self.palette
            .input
            .update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
        cx.notify();
    }

    fn notify_user(&mut self, msg: SharedString, cx: &mut Context<Self>) {
        self.notice = Some(msg);
        cx.notify();
    }

    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        self.restore_focus(window, cx);
        cx.notify();
    }

    /// Give keyboard focus back to the page after an overlay closes.
    fn restore_focus(&self, window: &mut Window, cx: &mut App) {
        if self.page == Page::Terminal {
            if let Some(tab) = self.active_tab.and_then(|id| self.tabs.iter().find(|t| t.id == id)) {
                match &tab.content {
                    TabContent::Terminal(_) => window.focus(&self.term_focus, cx),
                    TabContent::Desktop(pane) => window.focus(&pane.read(cx).focus_handle(), cx),
                }
                return;
            }
        }
        if self.page == Page::Sftp {
            window.focus(&self.files.read(cx).focus_handle(), cx);
        } else {
            window.focus(&self.root_focus, cx);
        }
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        match SessionStore::open_default() {
            Ok(store) => {
                self.sessions = store.list().to_vec();
                self.store_error = None;
                let hosts = self.sessions.clone();
                self.files.update(cx, |files, cx| files.set_hosts(hosts, cx));
                let msg = i18n::tr_count("app.status_sessions", self.sessions.len());
                self.notify_user(msg, cx);
            }
            Err(err) => {
                self.store_error = Some(SharedString::from(err.to_string()));
                cx.notify();
            }
        }
    }

    /// Enter in the search box / CONNECT: a single matching saved host wins,
    /// otherwise the text is parsed as a quick-connect target.
    fn connect_from_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().trim().to_string();
        let matches = self.filtered_sessions(cx);
        if !query.is_empty() && matches.len() == 1 {
            let profile = matches[0].clone();
            self.connect_profile(profile, window, cx);
            return;
        }
        match parse_quick_connect(&query) {
            Some(profile) => self.connect_profile(profile, window, cx),
            None => self.notify_user(i18n::tr("app.quick_connect_hint"), cx),
        }
    }

    fn connect_profile(&mut self, profile: SessionProfile, window: &mut Window, cx: &mut Context<Self>) {
        match profile.protocol {
            Protocol::Local => {
                let _ = self.open_local(window, cx);
                return;
            }
            Protocol::Sftp | Protocol::Ftp => {
                self.show_page(Page::Sftp, window, cx);
                self.files
                    .update(cx, |files, cx| files.connect(profile, window, cx));
                return;
            }
            Protocol::Vnc => {
                self.open_vnc(profile, window, cx);
                return;
            }
            Protocol::Rdp => {
                self.notify_user(i18n::tr("app.remote_desktop_soon"), cx);
                return;
            }
            _ => {}
        }

        let store = SessionStore::open_default().ok();
        let target = match RemoteTarget::from_profile(&profile, store.as_ref(), &self.prompts) {
            Ok(target) => target,
            Err(err) => {
                self.notify_user(i18n::tr_error("app.connect_failed", &format!("{err:#}")), cx);
                return;
            }
        };

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let owner = cx.entity().downgrade();
        let tab = RemoteTerminalTab::connect(
            target,
            session_target(&profile),
            move |_, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == id) {
                        tab.closed = true;
                        cx.notify();
                    }
                });
            },
            window,
            cx,
        );
        self.attach_pane(&tab.pane, cx);
        self.tabs.push(OpenTab {
            id,
            title: SharedString::from(profile.name.clone()),
            protocol: profile.protocol,
            content: TabContent::Terminal(tab.pane.clone()),
            backend: TabBackend::Remote(tab),
            closed: false,
        });
        self.active_tab = Some(id);
        self.show_page(Page::Terminal, window, cx);
    }

    fn open_vnc(&mut self, profile: SessionProfile, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let owner = cx.entity().downgrade();
        let tab = match crate::remote_desktop::RemoteDesktopTab::connect(
            profile.clone(),
            self.prompts.clone(),
            move |_, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == id) {
                        tab.closed = true;
                        cx.notify();
                    }
                });
            },
            window,
            cx,
        ) {
            Ok(tab) => tab,
            Err(err) => {
                self.notify_user(i18n::tr_error("app.connect_failed", &format!("{err:#}")), cx);
                return;
            }
        };
        self.tabs.push(OpenTab {
            id,
            title: SharedString::from(profile.name),
            protocol: Protocol::Vnc,
            content: TabContent::Desktop(tab.pane.clone()),
            backend: TabBackend::Desktop(tab),
            closed: false,
        });
        self.active_tab = Some(id);
        self.show_page(Page::Terminal, window, cx);
    }

    fn push_prompt(&mut self, req: PromptRequest, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_queue.push_back(req);
        if self.prompt_queue.len() == 1 {
            self.present_prompt(window, cx);
        }
        cx.notify();
    }

    /// Prepare the UI for the prompt now at the front of the queue.
    fn present_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.prompt_queue.front() {
            Some(PromptRequest::Secret { remember, .. }) => {
                self.remember_secret = remember.unwrap_or(false);
                self.secret_input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
            }
            Some(PromptRequest::HostKey { .. } | PromptRequest::Certificate { .. }) => {}
            None => self.restore_focus(window, cx),
        }
        cx.notify();
    }

    /// Answer the host-key or certificate question at the front of the queue.
    fn answer_trust(&mut self, decision: HostKeyDecision, window: &mut Window, cx: &mut Context<Self>) {
        match self.prompt_queue.front() {
            Some(PromptRequest::HostKey { .. } | PromptRequest::Certificate { .. }) => {}
            _ => return,
        }
        match self.prompt_queue.pop_front() {
            Some(PromptRequest::HostKey { reply, .. }) => {
                let _ = reply.send(decision);
            }
            Some(PromptRequest::Certificate { reply, .. }) => {
                let _ = reply.send(match decision {
                    HostKeyDecision::Reject => CertDecision::Reject,
                    HostKeyDecision::AcceptOnce => CertDecision::AcceptOnce,
                    HostKeyDecision::AcceptAndLearn => CertDecision::AcceptAndSave,
                });
            }
            _ => {}
        }
        self.present_prompt(window, cx);
    }

    fn answer_secret(&mut self, accept: bool, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.prompt_queue.front(), Some(PromptRequest::Secret { .. })) {
            let value = self.secret_input.read(cx).value().to_string();
            self.secret_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            if let Some(PromptRequest::Secret { reply, remember, .. }) = self.prompt_queue.pop_front()
            {
                let answer = SecretAnswer {
                    value,
                    remember: remember.is_some() && self.remember_secret,
                };
                let _ = reply.send(accept.then_some(answer));
            }
            self.present_prompt(window, cx);
        }
    }

    fn open_local(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let tab = match LocalTerminalTab::spawn(window, cx) {
            Ok(tab) => tab,
            Err(err) => {
                let msg = i18n::tr_error("app.status_local_failed", &err.to_string());
                self.notify_user(msg, cx);
                return false;
            }
        };

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let pane = tab.pane.clone();
        self.attach_pane(&pane, cx);
        self.tabs.push(OpenTab {
            id,
            title: i18n::tr("app.local_tab"),
            protocol: Protocol::Local,
            content: TabContent::Terminal(pane),
            backend: TabBackend::Local(tab),
            closed: false,
        });
        self.active_tab = Some(id);
        self.show_page(Page::Terminal, window, cx);
        true
    }

    fn close_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.retain(|t| t.id != id);
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|t| t.id);
        }
        if self.tabs.is_empty() && self.page == Page::Terminal {
            self.page = Page::Vault;
        }
        self.restore_focus(window, cx);
        cx.notify();
    }

    fn select_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active_tab = Some(id);
        self.show_page(Page::Terminal, window, cx);
    }

    /// Activate the tab `step` positions away (wrapping).
    fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as isize;
        let next = match self.active_tab.and_then(|id| self.tabs.iter().position(|t| t.id == id)) {
            Some(ix) if self.page == Page::Terminal => (ix as isize + step).rem_euclid(len),
            Some(ix) => ix as isize,
            None => 0,
        };
        let id = self.tabs[next as usize].id;
        self.select_tab(id, window, cx);
    }

    fn attach_pane(&self, pane: &Entity<TerminalPane>, cx: &mut App) {
        let focus = self.term_focus.clone();
        pane.update(cx, |pane, _| pane.set_focus_handle(focus));
    }

    fn copy_from_terminal(&self, cx: &mut App) {
        let text = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
            .and_then(|tab| tab.terminal())
            .and_then(|pane| pane.read(cx).selection_text());
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn active_open_tab(&self) -> Option<&OpenTab> {
        let id = self.active_tab?;
        self.tabs.iter().find(|t| t.id == id && !t.closed)
    }

    fn paste_into_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some(tab) = self.active_open_tab() else {
            return;
        };
        let Some(pane) = tab.terminal().cloned() else {
            return;
        };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        pane.update(cx, |pane, cx| pane.on_user_input(cx));
        if pane.read(cx).input_modes().bracketed_paste {
            let body = text.replace('\x1b', "");
            tab.backend
                .write_input(format!("\x1b[200~{body}\x1b[201~").as_bytes());
        } else {
            tab.backend.write_input(text.as_bytes());
        }
    }

    fn on_close_tab(&mut self, _: &menus::CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.open {
            self.close_palette(window, cx);
        } else if self.page == Page::Terminal {
            if let Some(id) = self.active_tab {
                self.close_tab(id, window, cx);
            }
        }
    }

    fn filtered_sessions(&self, cx: &App) -> Vec<&SessionProfile> {
        let query = self.search.read(cx).value().to_lowercase();
        let query = query.trim();
        self.sessions
            .iter()
            .filter(|s| {
                query.is_empty()
                    || s.name.to_lowercase().contains(query)
                    || session_target(s).to_lowercase().contains(query)
            })
            .collect()
    }

    fn send_key(&self, event: &KeyDownEvent, cx: &mut App) {
        let Some(tab) = self.active_open_tab() else {
            return;
        };
        let Some(pane) = tab.terminal().cloned() else {
            return;
        };
        let backend = &tab.backend;
        let ks = &event.keystroke;
        if ks.modifiers.platform {
            return;
        }
        pane.update(cx, |pane, cx| pane.on_user_input(cx));
        if ks.modifiers.control {
            let key = ks.key.as_bytes();
            if key.len() == 1 && key[0].is_ascii_alphabetic() {
                backend.write_input(&[key[0].to_ascii_lowercase() - b'a' + 1]);
                return;
            }
        }
        let app_cursor = pane.read(cx).input_modes().app_cursor;
        let cursor = |normal: &'static [u8], app: &'static [u8]| if app_cursor { app } else { normal };
        let seq: &[u8] = match ks.key.as_str() {
            "enter" => b"\r",
            "backspace" => &[0x7f],
            "tab" if ks.modifiers.shift => b"\x1b[Z",
            "tab" => b"\t",
            "escape" => b"\x1b",
            "up" => cursor(b"\x1b[A", b"\x1bOA"),
            "down" => cursor(b"\x1b[B", b"\x1bOB"),
            "right" => cursor(b"\x1b[C", b"\x1bOC"),
            "left" => cursor(b"\x1b[D", b"\x1bOD"),
            "home" => cursor(b"\x1b[H", b"\x1bOH"),
            "end" => cursor(b"\x1b[F", b"\x1bOF"),
            "delete" => b"\x1b[3~",
            "pageup" => b"\x1b[5~",
            "pagedown" => b"\x1b[6~",
            _ => {
                if let Some(ch) = ks.key_char.as_ref() {
                    backend.write_input(ch.as_bytes());
                }
                return;
            }
        };
        backend.write_input(seq);
    }
}

/// Title, host, hint, detail line and fingerprint for a trust prompt.
fn trust_text(req: &PromptRequest) -> (SharedString, String, SharedString, String, String) {
    match req {
        PromptRequest::Certificate { info, .. } => (
            i18n::tr("app.cert_title"),
            format!("{}:{}", info.host, info.port),
            i18n::tr("app.cert_hint"),
            info.reason.clone(),
            info.fingerprint.clone(),
        ),
        PromptRequest::HostKey { info, .. } => (
            i18n::tr("app.host_key_title"),
            info.display_host(),
            i18n::tr("app.host_key_hint"),
            info.algorithm.clone(),
            info.fingerprint.clone(),
        ),
        PromptRequest::Secret { .. } => Default::default(),
    }
}

pub(crate) fn session_target(s: &SessionProfile) -> String {
    match s.protocol {
        Protocol::Ssh => s
            .ssh
            .as_ref()
            .map(|c| format!("{}@{}:{}", c.username, c.host, c.port))
            .unwrap_or_default(),
        Protocol::Telnet => s
            .telnet
            .as_ref()
            .map(|c| format!("{}:{}", c.host, c.port))
            .unwrap_or_default(),
        Protocol::Tcp => s
            .tcp
            .as_ref()
            .map(|c| format!("{}:{}", c.host, c.port))
            .unwrap_or_default(),
        Protocol::Serial => s
            .serial
            .as_ref()
            .map(|c| format!("{} @ {}", c.device, c.baud_rate))
            .unwrap_or_default(),
        Protocol::Ftp => s
            .ftp
            .as_ref()
            .map(|c| format!("{}@{}:{}", c.username, c.host, c.port))
            .unwrap_or_default(),
        Protocol::Vnc => s
            .vnc
            .as_ref()
            .map(|c| format!("{}:{}", c.host, c.port))
            .unwrap_or_default(),
        Protocol::Rdp => s
            .rdp
            .as_ref()
            .map(|c| format!("{}@{}:{}", c.username, c.host, c.port))
            .unwrap_or_default(),
        Protocol::Local => "local-shell".into(),
        other => other.as_str().into(),
    }
}

/// Parse `[ssh ]user@host[:port]`, `host`, `telnet host[:port]`,
/// `ftp://` / `ftps://` / `ftp user@host`, `vnc://host[:port]`, or
/// `rdp://user@host[:port]` into an unsaved profile.
pub(crate) fn parse_quick_connect(input: &str) -> Option<SessionProfile> {
    let input = input.trim();
    let (kind, rest) = if let Some(rest) = input.strip_prefix("telnet ") {
        (Some(Protocol::Telnet), rest)
    } else if let Some(rest) = input.strip_prefix("ftps://") {
        (Some(Protocol::Ftp), rest)
    } else if let Some(rest) = input
        .strip_prefix("ftp://")
        .or_else(|| input.strip_prefix("ftp "))
    {
        (Some(Protocol::Ftp), rest)
    } else if let Some(rest) = input
        .strip_prefix("vnc://")
        .or_else(|| input.strip_prefix("vnc "))
    {
        (Some(Protocol::Vnc), rest)
    } else if let Some(rest) = input
        .strip_prefix("rdp://")
        .or_else(|| input.strip_prefix("rdp "))
    {
        (Some(Protocol::Rdp), rest)
    } else {
        (None, input.strip_prefix("ssh ").unwrap_or(input))
    };
    let rest = rest.trim();
    let rest = if matches!(kind, Some(Protocol::Ftp | Protocol::Vnc | Protocol::Rdp)) {
        rest.trim_end_matches('/')
    } else {
        rest
    };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return None;
    }

    let (user, hostport) = match rest.rsplit_once('@') {
        Some((user, hp)) if !user.is_empty() => (Some(user.to_string()), hp),
        Some(_) => return None,
        None => (None, rest),
    };
    let (host, port) = split_host_port(hostport)?;

    match kind {
        Some(Protocol::Telnet) => {
            return Some(SessionProfile::new_telnet(host.clone(), host, port.unwrap_or(23)));
        }
        Some(Protocol::Ftp) => {
            let security = if input.starts_with("ftps://") {
                FtpSecurity::Explicit
            } else {
                FtpSecurity::Auto
            };
            let user = user.unwrap_or_else(|| "anonymous".into());
            let name = format!("ftp://{user}@{host}");
            return Some(SessionProfile::new_ftp(name, host, user, port.unwrap_or(21), security));
        }
        Some(Protocol::Vnc) => {
            let name = format!("vnc://{host}");
            return Some(SessionProfile::new_vnc(name, host, port.unwrap_or(5900)));
        }
        Some(Protocol::Rdp) => {
            let user = user.unwrap_or_else(|| "Administrator".into());
            let name = format!("rdp://{user}@{host}");
            return Some(SessionProfile::new_rdp(name, host, user, port.unwrap_or(3389)));
        }
        Some(_) | None => {}
    }
    let user = user
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".into());
    let has_default_key = dirs::home_dir().is_some_and(|home| {
        ["id_ed25519", "id_rsa", "id_ecdsa"]
            .iter()
            .any(|name| home.join(".ssh").join(name).exists())
    });
    let auth = SshAuth {
        method: if has_default_key {
            AuthMethod::PublicKey
        } else {
            AuthMethod::Password
        },
        password: None,
        private_key_path: None,
        passphrase: None,
    };
    let name = format!("{user}@{host}");
    Some(SessionProfile::new_ssh(name, host, user, port.unwrap_or(22), auth))
}

/// `host`, `host:port`, `[v6]:port`; bare IPv6 without brackets is taken as host.
fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse().ok()?),
            None if tail.is_empty() => None,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    match s.split_once(':') {
        Some((host, port)) if !port.contains(':') => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_string(), Some(port.parse().ok()?)))
        }
        _ if s.is_empty() => None,
        _ => Some((s.to_string(), None)),
    }
}

fn protocol_icon(p: Protocol) -> IconName {
    match p {
        Protocol::Serial => IconName::Network,
        Protocol::Sftp | Protocol::Ftp => IconName::Folder,
        Protocol::Rdp | Protocol::Vnc => IconName::LayoutDashboard,
        _ => IconName::SquareTerminal,
    }
}

fn protocol_color(p: Protocol) -> Hsla {
    match p {
        Protocol::Ssh => rgb(0x6c5ce7),
        Protocol::Telnet => rgb(0xd19a66),
        Protocol::Serial => rgb(0x2fb57f),
        Protocol::Tcp => rgb(0x3d8fd6),
        Protocol::Local => rgb(0x5c6378),
        Protocol::Ftp => rgb(0x2a9d8f),
        Protocol::Vnc => rgb(0xe76f51),
        Protocol::Rdp => rgb(0x457b9d),
        _ => rgb(0xb05fc9),
    }
    .into()
}

impl Render for GaleApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let body = match self.page {
            Page::Vault => self.render_vault(cx).into_any_element(),
            Page::Sftp => div()
                .size_full()
                .rounded_tl(px(12.))
                .overflow_hidden()
                .child(self.files.clone())
                .into_any_element(),
            Page::Terminal => self.render_terminal(window, cx).into_any_element(),
        };

        let palette = self.render_palette(cx);
        let prompt = self.render_prompt(cx);

        div()
            .id("gale-root")
            .key_context("GaleApp")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|this, _: &menus::ToggleCommandPalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &menus::QuickConnect, window, cx| {
                if !this.palette.open {
                    this.toggle_palette(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &menus::NewLocalTerminal, window, cx| {
                let _ = this.open_local(window, cx);
            }))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(|this, _: &menus::NextTab, window, cx| {
                this.cycle_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &menus::PrevTab, window, cx| {
                this.cycle_tab(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &menus::ShowVault, window, cx| {
                this.show_page(Page::Vault, window, cx);
            }))
            .on_action(cx.listener(|this, _: &menus::ShowSftp, window, cx| {
                this.show_page(Page::Sftp, window, cx);
            }))
            .on_action(cx.listener(|this, _: &menus::OpenSettings, window, cx| {
                this.section = VaultSection::Settings;
                this.show_page(Page::Vault, window, cx);
            }))
            .on_action(cx.listener(|this, _: &menus::RefreshHosts, _, cx| {
                this.refresh_sessions(cx);
            }))
            .on_action(|_: &menus::Minimize, window, _| window.minimize_window())
            .on_action(|_: &menus::ToggleFullScreen, window, _| window.toggle_fullscreen())
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.title_bar)
            .text_color(theme.foreground)
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(body)
                    .children(self.notice.clone().map(|msg| {
                        div()
                            .id("notice")
                            .absolute()
                            .bottom_4()
                            .right_4()
                            .max_w(px(420.))
                            .px_4()
                            .py_2p5()
                            .rounded(px(10.))
                            .bg(theme.popover)
                            .border_1()
                            .border_color(theme.border)
                            .shadow_lg()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .child(
                                Icon::new(IconName::Info)
                                    .small()
                                    .text_color(theme.primary),
                            )
                            .child(Label::new(msg).text_sm())
                            .child(
                                Button::new("notice-close")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.notice = None;
                                        cx.notify();
                                    })),
                            )
                    })),
            )
            .children(palette)
            .children(prompt)
    }
}

impl GaleApp {
    /// Modal for the prompt at the front of the queue (host key / secret).
    fn render_prompt(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let front = self.prompt_queue.front()?;

        let card = div()
            .w(px(460.))
            .p_5()
            .rounded(px(14.))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_4();

        let actions = div().flex().flex_row().justify_end().gap_2();

        let card = match front {
            PromptRequest::HostKey { .. } | PromptRequest::Certificate { .. } => {
                let (title, host, hint, detail, fingerprint) = trust_text(front);
                card
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(Icon::new(IconName::Globe).text_color(theme.warning))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    Label::new(title)
                                        .text_base()
                                        .font_weight(FontWeight::SEMIBOLD),
                                )
                                .child(
                                    Label::new(host)
                                        .text_sm()
                                        .text_color(theme.muted_foreground),
                                ),
                        ),
                )
                .child(
                    Label::new(hint)
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .p_3()
                        .rounded(px(8.))
                        .bg(theme.secondary)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(Label::new(detail).text_color(theme.muted_foreground))
                        .child(Label::new(fingerprint)),
                )
                .child(
                    actions
                        .child(
                            Button::new("hostkey-cancel")
                                .ghost()
                                .label(i18n::tr("app.cancel"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.answer_trust(HostKeyDecision::Reject, window, cx);
                                })),
                        )
                        .child(
                            Button::new("hostkey-once")
                                .secondary()
                                .label(i18n::tr("app.accept_once"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.answer_trust(HostKeyDecision::AcceptOnce, window, cx);
                                })),
                        )
                        .child(
                            Button::new("hostkey-save")
                                .primary()
                                .label(i18n::tr("app.accept_save"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.answer_trust(HostKeyDecision::AcceptAndLearn, window, cx);
                                })),
                        ),
                )
            }
            PromptRequest::Secret {
                prompt,
                remember,
                stale,
                ..
            } => card
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(Icon::new(IconName::User).text_color(theme.primary))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(
                                    Label::new(i18n::tr("app.auth_title"))
                                        .text_base()
                                        .font_weight(FontWeight::SEMIBOLD),
                                )
                                .child(
                                    Label::new(prompt.clone())
                                        .text_sm()
                                        .text_color(theme.muted_foreground),
                                ),
                        ),
                )
                .when(*stale, |s| {
                    s.child(
                        Label::new(i18n::tr("app.saved_secret_rejected"))
                            .text_sm()
                            .text_color(theme.warning),
                    )
                })
                .child(Input::new(&self.secret_input).large().mask_toggle())
                .when(remember.is_some(), |s| {
                    s.child(
                        Checkbox::new("secret-remember")
                            .label(i18n::tr("app.remember_secret"))
                            .checked(self.remember_secret)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.remember_secret = *checked;
                                cx.notify();
                            })),
                    )
                })
                .child(
                    actions
                        .child(
                            Button::new("secret-cancel")
                                .ghost()
                                .label(i18n::tr("app.cancel"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.answer_secret(false, window, cx);
                                })),
                        )
                        .child(
                            Button::new("secret-ok")
                                .primary()
                                .label(i18n::tr("app.continue"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.answer_secret(true, window, cx);
                                })),
                        ),
                ),
        };

        Some(
            div()
                .id("prompt-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .bg(hsla(0., 0., 0., 0.45))
                .flex()
                .items_center()
                .justify_center()
                .child(card)
                .into_any_element(),
        )
    }

    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let page = self.page;
        let active = self.active_tab;

        let nav_tab = |id: &'static str,
                       icon: IconName,
                       label: SharedString,
                       selected: bool,
                       target: Page,
                       cx: &mut Context<Self>| {
            div()
                .id(id)
                .h(px(30.))
                .px_3()
                .rounded(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_sm()
                .when(selected, |s| s.bg(theme.tab_active).text_color(theme.tab_active_foreground))
                .when(!selected, |s| {
                    s.text_color(theme.tab_foreground)
                        .hover(|s| s.bg(theme.secondary).text_color(theme.foreground))
                })
                .child(Icon::new(icon).small())
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        window.prevent_default();
                        this.show_page(target, window, cx);
                    }),
                )
        };

        let vault_tab = nav_tab(
            "nav-vault",
            IconName::LayoutDashboard,
            i18n::tr("app.vault"),
            page == Page::Vault,
            Page::Vault,
            cx,
        );
        let sftp_tab = nav_tab(
            "nav-sftp",
            IconName::Folder,
            i18n::tr("app.sftp"),
            page == Page::Sftp,
            Page::Sftp,
            cx,
        );

        let conn_tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .map(|tab| {
                let id = tab.id;
                let selected = page == Page::Terminal && active == Some(id);
                div()
                    .id(SharedString::from(format!("tab-{id}")))
                    .h(px(30.))
                    .min_w(px(120.))
                    .max_w(px(200.))
                    .pl_3()
                    .pr_1()
                    .rounded(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .when(selected, |s| {
                        s.bg(theme.tab_active).text_color(theme.tab_active_foreground)
                    })
                    .when(!selected, |s| {
                        s.text_color(theme.tab_foreground)
                            .hover(|s| s.bg(theme.secondary).text_color(theme.foreground))
                    })
                    .child(
                        div()
                            .w(px(18.))
                            .h(px(18.))
                            .rounded(px(5.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(protocol_color(tab.protocol))
                            .when(tab.closed, |s| s.opacity(0.35))
                            .child(Icon::new(protocol_icon(tab.protocol)).xsmall().text_color(white())),
                    )
                    .child(div().flex_1().overflow_hidden().child(tab.title.clone()))
                    .child(
                        Button::new(SharedString::from(format!("close-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_tab(id, window, cx);
                            })),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            window.prevent_default();
                            this.select_tab(id, window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        TitleBar::new()
            .h(px(TITLE_BAR_H))
            .bg(theme.title_bar)
            .border_b_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(vault_tab)
                    .child(sftp_tab)
                    .when(!conn_tabs.is_empty(), |s| {
                        s.child(div().mx_1().w(px(1.)).h(px(18.)).bg(theme.border))
                    })
                    .children(conn_tabs)
                    .child(
                        Button::new("tab-new")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let _ = this.open_local(window, cx);
                            })),
                    ),
            )
            .child(div().w(px(12.)))
    }

    fn render_vault(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .size_full()
            .flex()
            .flex_row()
            .child(self.render_vault_nav(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(theme.background)
                    .rounded_tl(px(12.))
                    .child(match self.section {
                        VaultSection::Hosts => self.render_hosts(cx).into_any_element(),
                        VaultSection::Settings => self.render_settings(cx).into_any_element(),
                        VaultSection::Keychain => self.render_keychain(cx).into_any_element(),
                        other => self
                            .render_placeholder(other.icon(), i18n::tr(other.label_key()), cx)
                            .into_any_element(),
                    }),
            )
    }

    fn render_vault_nav(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let current = self.section;

        let item = |section: VaultSection, cx: &mut Context<Self>| {
            let selected = current == section;
            div()
                .id(SharedString::from(format!("vault-nav-{}", section.label_key())))
                .h(px(34.))
                .px_3()
                .rounded(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .text_sm()
                .when(selected, |s| {
                    s.bg(theme.sidebar_accent)
                        .text_color(theme.sidebar_accent_foreground)
                })
                .when(!selected, |s| {
                    s.text_color(theme.sidebar_foreground)
                        .hover(|s| s.bg(theme.list_hover))
                })
                .child(
                    Icon::new(section.icon())
                        .small()
                        .text_color(if selected { theme.primary } else { theme.muted_foreground }),
                )
                .child(i18n::tr(section.label_key()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.section = section;
                    if section == VaultSection::Keychain {
                        this.reload_credentials();
                    }
                    cx.notify();
                }))
        };

        div()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2()
            .pt_2()
            .pb_3()
            .bg(theme.title_bar)
            .child(item(VaultSection::Hosts, cx))
            .child(item(VaultSection::Keychain, cx))
            .child(item(VaultSection::PortForwarding, cx))
            .child(item(VaultSection::Snippets, cx))
            .child(item(VaultSection::KnownHosts, cx))
            .child(div().flex_1())
            .child(item(VaultSection::Settings, cx))
    }

    fn render_hosts(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let hosts: Vec<SessionProfile> = self.filtered_sessions(cx).into_iter().cloned().collect();
        let has_any = !self.sessions.is_empty();

        let toolbar = div()
            .flex()
            .flex_col()
            .gap_3()
            .px_6()
            .pt_5()
            .pb_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.search)
                                .large()
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).small()),
                        ),
                    )
                    .child(
                        Button::new("search-connect")
                            .primary()
                            .large()
                            .label(i18n::tr("app.connect"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.connect_from_search(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("new-host")
                            .secondary()
                            .small()
                            .icon(IconName::Plus)
                            .label(i18n::tr("app.new_host"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notify_user(i18n::tr("app.remote_todo"), cx);
                            })),
                    )
                    .child(
                        Button::new("new-terminal")
                            .secondary()
                            .small()
                            .icon(IconName::SquareTerminal)
                            .label(i18n::tr("app.local_terminal"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let _ = this.open_local(window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("refresh-hosts")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_sessions(cx))),
                    ),
            );

        let content = if !has_any {
            self.empty_state(
                IconName::HardDrive,
                i18n::tr("app.empty_vault_title"),
                i18n::tr("app.empty_vault_hint"),
                Some(
                    Button::new("empty-terminal")
                        .primary()
                        .icon(IconName::SquareTerminal)
                        .label(i18n::tr("app.local_terminal"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let _ = this.open_local(window, cx);
                        }))
                        .into_any_element(),
                ),
                cx,
            )
            .into_any_element()
        } else if hosts.is_empty() {
            self.empty_state(IconName::Search, i18n::tr("app.no_match"), "".into(), None, cx)
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            Label::new(i18n::tr("app.hosts"))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(hosts.len().to_string())
                                .text_xs()
                                .text_color(theme.muted_foreground),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_3()
                        .children(hosts.iter().map(|s| self.host_card(s, cx))),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .id("hosts-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_5()
                    .child(content),
            )
    }

    fn host_card(&self, s: &SessionProfile, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let subtitle = match s.protocol {
            Protocol::Ssh => s
                .ssh
                .as_ref()
                .map(|c| format!("ssh, {}", c.username))
                .unwrap_or_else(|| "ssh".into()),
            p => format!("{}, {}", p.as_str(), session_target(s)),
        };

        div()
            .id(SharedString::from(format!("host-{}", s.id)))
            .w(px(300.))
            .h(px(68.))
            .px_3()
            .rounded(px(12.))
            .bg(theme.secondary)
            .hover(|style| style.bg(theme.secondary_hover))
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(40.))
                    .h(px(40.))
                    .flex_shrink_0()
                    .rounded(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(protocol_color(s.protocol))
                    .child(Icon::new(protocol_icon(s.protocol)).text_color(white())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        Label::new(s.name.clone())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(subtitle)
                            .text_xs()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .on_click({
                let profile = s.clone();
                cx.listener(move |this, _, window, cx| {
                    this.connect_profile(profile.clone(), window, cx);
                })
            })
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let locale_pref = self.locale_pref;

        div()
            .size_full()
            .px_6()
            .py_5()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                Label::new(i18n::tr("app.settings"))
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                div()
                    .max_w(px(560.))
                    .p_4()
                    .rounded(px(12.))
                    .bg(theme.secondary)
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(Label::new(i18n::tr("app.language")).text_sm())
                    .child(
                        div().flex().flex_row().gap_1().children(
                            [
                                (LocalePreference::Auto, "app.language_auto"),
                                (LocalePreference::En, "app.language_en"),
                                (LocalePreference::ZhCn, "app.language_zh_cn"),
                            ]
                            .into_iter()
                            .map(|(pref, key)| {
                                let selected = locale_pref == pref;
                                Button::new(SharedString::from(format!(
                                    "locale-{}",
                                    pref.as_storage_key()
                                )))
                                .small()
                                .when(selected, |b| b.primary())
                                .when(!selected, |b| b.ghost())
                                .label(i18n::tr(key))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_locale_pref(pref, window, cx);
                                }))
                            }),
                        ),
                    ),
            )
            .children(self.store_error.clone().map(|err| {
                Label::new(err)
                    .text_xs()
                    .text_color(theme.danger)
            }))
    }

    fn render_placeholder(
        &mut self,
        icon: IconName,
        title: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .size_full()
            .bg(theme.background)
            .child(self.empty_state(
                icon,
                SharedString::from(format!("{title} · {}", i18n::tr("app.coming_soon"))),
                i18n::tr("app.coming_soon_hint"),
                None,
                cx,
            ))
    }

    fn empty_state(
        &self,
        icon: IconName,
        title: SharedString,
        hint: SharedString,
        action: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .size_full()
            .min_h(px(320.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .w(px(64.))
                    .h(px(64.))
                    .rounded(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme.accent)
                    .child(Icon::new(icon).large().text_color(theme.primary)),
            )
            .child(
                Label::new(title)
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .when(!hint.is_empty(), |s| {
                s.child(
                    div()
                        .max_w(px(380.))
                        .text_center()
                        .child(Label::new(hint).text_sm().text_color(theme.muted_foreground)),
                )
            })
            .children(action)
    }

    fn render_terminal(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;
        let tab = self.tabs.iter().find(|t| Some(t.id) == active);
        let is_desktop = tab.is_some_and(|t| t.backend.is_desktop());
        let content = tab.map(|t| match &t.content {
            TabContent::Terminal(pane) => pane.clone().into_any_element(),
            TabContent::Desktop(pane) => pane.clone().into_any_element(),
        });
        let bg = cx.theme().background;

        div()
            .id("terminal-stage")
            .key_context(menus::TERMINAL_CONTEXT)
            .track_focus(&self.term_focus)
            .size_full()
            .bg(bg)
            .when(!is_desktop, |s| {
                s.on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.send_key(event, cx)))
                    .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste_into_terminal(cx)))
                    .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy_from_terminal(cx)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| window.focus(&this.term_focus, cx)),
                    )
            })
            .map(|s| match content {
                Some(pane) => s.child(pane),
                None => s.child(
                    self.empty_state(
                        IconName::SquareTerminal,
                        i18n::tr("app.empty_terminal"),
                        i18n::tr("app.empty_terminal_hint"),
                        Some(
                            Button::new("empty-open-local")
                                .primary()
                                .icon(IconName::SquareTerminal)
                                .label(i18n::tr("app.local_terminal"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let _ = this.open_local(window, cx);
                                }))
                                .into_any_element(),
                        ),
                        cx,
                    ),
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_quick_connect, FtpSecurity, Protocol};

    #[test]
    fn quick_connect_parses_ssh_targets() {
        let p = parse_quick_connect("ssh alice@example.com:2222").unwrap();
        let ssh = p.ssh.unwrap();
        assert_eq!(p.protocol, Protocol::Ssh);
        assert_eq!((ssh.username.as_str(), ssh.host.as_str(), ssh.port), ("alice", "example.com", 2222));

        let ssh = parse_quick_connect("bob@[::1]").unwrap().ssh.unwrap();
        assert_eq!((ssh.host.as_str(), ssh.port), ("::1", 22));
    }

    #[test]
    fn quick_connect_parses_telnet_and_rejects_junk() {
        let telnet = parse_quick_connect("telnet 10.0.0.1:2323").unwrap().telnet.unwrap();
        assert_eq!((telnet.host.as_str(), telnet.port), ("10.0.0.1", 2323));

        assert!(parse_quick_connect("").is_none());
        assert!(parse_quick_connect("two words").is_none());
        assert!(parse_quick_connect("host:notaport").is_none());
        assert!(parse_quick_connect("@host").is_none());
    }

    #[test]
    fn quick_connect_parses_ftp_urls() {
        let p = parse_quick_connect("ftps://tester@files.lan:2990/").unwrap();
        let ftp = p.ftp.unwrap();
        assert_eq!(p.protocol, Protocol::Ftp);
        assert_eq!((ftp.username.as_str(), ftp.host.as_str(), ftp.port), ("tester", "files.lan", 2990));
        assert_eq!(ftp.security, FtpSecurity::Explicit);

        let ftp = parse_quick_connect("ftp 10.0.0.9").unwrap().ftp.unwrap();
        assert_eq!((ftp.username.as_str(), ftp.port, ftp.security), ("anonymous", 21, FtpSecurity::Auto));
    }

    #[test]
    fn quick_connect_parses_vnc_and_rdp() {
        let vnc = parse_quick_connect("vnc://10.0.0.5:5901").unwrap();
        assert_eq!(vnc.protocol, Protocol::Vnc);
        assert_eq!(vnc.vnc.unwrap().port, 5901);

        let rdp = parse_quick_connect("rdp alice@desk.lan").unwrap();
        assert_eq!(rdp.protocol, Protocol::Rdp);
        let cfg = rdp.rdp.unwrap();
        assert_eq!((cfg.username.as_str(), cfg.host.as_str(), cfg.port), ("alice", "desk.lan", 3389));
    }
}
