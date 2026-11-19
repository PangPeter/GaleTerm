//! Dual-pane file manager (the SFTP page): local disk on the left, a remote
//! host on the right. Listing and transfers run on the shared network runtime;
//! transfer progress is reported through atomics the UI polls.

mod backend;

use std::collections::BTreeSet;
use std::future::Future;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gale_core::{Protocol, SessionProfile, SessionStore};
use gale_ftp::{FtpClient, FtpClientConfig};
use gale_ssh::SftpClient;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    input::{Escape, Input, InputEvent, InputState},
    label::Label,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    ActiveTheme, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Theme,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use tokio::sync::oneshot;

use crate::app::{parse_quick_connect, session_target};
use crate::file_icons;
use crate::i18n;
use crate::remote_session::{runtime, ssh_config, GuiPrompts};
use backend::{format_mode, format_size, transfer, Backend, FileEntry};

const ROW_H: f32 = 28.;
const PROGRESS_TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Local,
    Remote,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Local,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Modified,
    Size,
}

struct Pane {
    backend: Option<Backend>,
    path: String,
    all: Vec<FileEntry>,
    /// `all` filtered (hidden files, quick filter) and sorted; selection
    /// indexes this.
    entries: Vec<FileEntry>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    /// Checkbox mode: plain clicks toggle rows instead of replacing the selection.
    multi: bool,
    sort: SortKey,
    ascending: bool,
    query: String,
    loading: bool,
    error: Option<SharedString>,
    show_hidden: bool,
    path_input: Entity<InputState>,
    filter_input: Entity<InputState>,
    scroll: UniformListScrollHandle,
    /// Bumped per listing request so stale results are dropped.
    request: u64,
}

impl Pane {
    fn new(backend: Option<Backend>, window: &mut Window, cx: &mut Context<FileManager>) -> Self {
        Self {
            backend,
            path: String::new(),
            all: Vec::new(),
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            multi: false,
            sort: SortKey::Name,
            ascending: true,
            query: String::new(),
            loading: false,
            error: None,
            show_hidden: false,
            path_input: cx.new(|cx| InputState::new(window, cx)),
            filter_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(i18n::tr("files.filter"))
            }),
            scroll: UniformListScrollHandle::new(),
            request: 0,
        }
    }

    /// Rebuild `entries` from `all`, keeping the selection on the same names.
    fn apply_filter(&mut self) {
        let selected: BTreeSet<String> = self
            .selected
            .iter()
            .filter_map(|&ix| self.entries.get(ix))
            .map(|e| e.name.clone())
            .collect();
        let anchor = self
            .anchor
            .and_then(|ix| self.entries.get(ix))
            .map(|e| e.name.clone());

        let query = self.query.to_lowercase();
        let mut entries: Vec<FileEntry> = self
            .all
            .iter()
            .filter(|e| self.show_hidden || !e.is_hidden())
            .filter(|e| query.is_empty() || e.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        let (key, ascending) = (self.sort, self.ascending);
        entries.sort_by(|a, b| {
            let order = match key {
                SortKey::Name => std::cmp::Ordering::Equal,
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Size => a.size.cmp(&b.size),
            }
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            let order = if ascending { order } else { order.reverse() };
            b.is_dir.cmp(&a.is_dir).then(order)
        });

        self.selected = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| selected.contains(&e.name))
            .map(|(ix, _)| ix)
            .collect();
        self.anchor = anchor.and_then(|name| entries.iter().position(|e| e.name == name));
        self.entries = entries;
    }

    fn select_only(&mut self, ix: usize) {
        self.selected = BTreeSet::from([ix]);
        self.anchor = Some(ix);
    }

    /// Selected entries with their full paths.
    fn selection(&self) -> Vec<(String, FileEntry)> {
        let Some(backend) = &self.backend else {
            return Vec::new();
        };
        self.selected
            .iter()
            .filter_map(|&ix| self.entries.get(ix))
            .map(|e| (backend.join(&self.path, &e.name), e.clone()))
            .collect()
    }

    fn reset(&mut self) {
        self.backend = None;
        self.path.clear();
        self.all.clear();
        self.entries.clear();
        self.selected.clear();
        self.anchor = None;
        self.multi = false;
        self.loading = false;
        self.error = None;
        self.request += 1;
    }
}

enum RemoteStatus {
    Disconnected,
    Connecting(SharedString),
    Connected(SharedString),
}

enum TransferState {
    Running,
    Done,
    Failed(SharedString),
    Cancelled,
}

struct Transfer {
    id: usize,
    name: SharedString,
    from: Side,
    total: Option<u64>,
    done: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    state: TransferState,
}

enum Dialog {
    NewFolder(Side),
    NewFile(Side),
    Rename { side: Side, from: String },
    Chmod { side: Side, items: Vec<(String, FileEntry)> },
    Delete { side: Side, items: Vec<(String, FileEntry)> },
    Overwrite { from: Side, items: Vec<(String, FileEntry)>, conflicts: Vec<String> },
}

/// Context-menu commands; each acts on the pane it was opened in.
#[derive(Clone, Copy)]
enum MenuCmd {
    Open,
    Transfer,
    OpenWithSystem,
    Reveal,
    CopyName,
    CopyPath,
    Rename,
    Chmod,
    Delete,
    NewFolder,
    NewFile,
    SelectAll,
    ToggleMulti,
    ToggleHidden,
    Refresh,
}

/// Payload while rows are dragged between panes.
#[derive(Clone)]
struct DraggedFiles {
    from: Side,
    items: Vec<(String, FileEntry)>,
}

struct DragPreview {
    label: SharedString,
    icon: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_3()
            .py_1p5()
            .rounded(px(8.))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.primary)
            .shadow_lg()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(theme.foreground)
            .child(img(self.icon.clone()).size(px(16.)))
            .child(self.label.clone())
    }
}

pub struct FileManager {
    local: Pane,
    remote: Pane,
    status: RemoteStatus,
    remote_error: Option<SharedString>,
    hosts: Vec<SessionProfile>,
    host_search: Entity<InputState>,
    prompts: GuiPrompts,
    transfers: Vec<Transfer>,
    next_transfer: usize,
    dialog: Option<Dialog>,
    dialog_input: Entity<InputState>,
    focus: FocusHandle,
    active: Side,
    /// Set by a row's right-click so the list behind it keeps the selection.
    row_menu: bool,
    _subscriptions: Vec<Subscription>,
}

impl FileManager {
    pub fn new(
        prompts: GuiPrompts,
        hosts: Vec<SessionProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let local = Pane::new(Some(Backend::Local), window, cx);
        let remote = Pane::new(None, window, cx);
        let host_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(i18n::tr("files.host_search"))
        });
        let dialog_input = cx.new(|cx| InputState::new(window, cx));

        let mut subscriptions = Vec::new();
        for (side, input) in [
            (Side::Local, local.filter_input.clone()),
            (Side::Remote, remote.filter_input.clone()),
        ] {
            subscriptions.push(cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let query = input.read(cx).value().trim().to_string();
                    let pane = this.pane_mut(side);
                    if pane.query != query {
                        pane.query = query;
                        pane.apply_filter();
                        cx.notify();
                    }
                }
            }));
        }
        for (side, input) in [
            (Side::Local, local.path_input.clone()),
            (Side::Remote, remote.path_input.clone()),
        ] {
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let path = input.read(cx).value().trim().to_string();
                        if !path.is_empty() {
                            this.navigate(side, path, window, cx);
                        }
                    }
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &host_search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.connect_from_search(window, cx),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &dialog_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.confirm_dialog(window, cx);
                }
            },
        ));

        let mut this = Self {
            local,
            remote,
            status: RemoteStatus::Disconnected,
            remote_error: None,
            hosts,
            host_search,
            prompts,
            transfers: Vec::new(),
            next_transfer: 1,
            dialog: None,
            dialog_input,
            focus: cx.focus_handle(),
            active: Side::Local,
            row_menu: false,
            _subscriptions: subscriptions,
        };
        let home = dirs::home_dir()
            .unwrap_or_else(|| "/".into())
            .to_string_lossy()
            .into_owned();
        this.navigate(Side::Local, home, window, cx);
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn set_hosts(&mut self, hosts: Vec<SessionProfile>, cx: &mut Context<Self>) {
        self.hosts = hosts;
        cx.notify();
    }

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Local => &self.local,
            Side::Remote => &self.remote,
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::Local => &mut self.local,
            Side::Remote => &mut self.remote,
        }
    }

    /// Run `fut` on the network runtime and hand its result back on the UI thread.
    fn spawn_io<T: Send + 'static>(
        window: &mut Window,
        cx: &mut Context<Self>,
        fut: impl Future<Output = anyhow::Result<T>> + Send + 'static,
        done: impl FnOnce(&mut Self, anyhow::Result<T>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (tx, rx) = oneshot::channel();
        runtime().spawn(async move {
            let _ = tx.send(fut.await);
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("background task stopped")));
            let _ = this.update_in(cx, |this, window, cx| done(this, result, window, cx));
        })
        .detach();
    }

    fn show_error(&mut self, side: Side, err: anyhow::Error, cx: &mut Context<Self>) {
        self.pane_mut(side).error = Some(SharedString::from(format!("{err:#}")));
        cx.notify();
    }

    // ---- navigation -------------------------------------------------------

    fn navigate(&mut self, side: Side, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        let Some(backend) = pane.backend.clone() else {
            return;
        };
        pane.request += 1;
        let request = pane.request;
        pane.loading = true;
        pane.error = None;
        let target = path.clone();
        Self::spawn_io(
            window,
            cx,
            async move { backend.list(&target).await },
            move |this, result, window, cx| {
                let pane = this.pane_mut(side);
                if pane.request != request {
                    return;
                }
                pane.loading = false;
                match result {
                    Ok(list) => {
                        let moved = pane.path != path;
                        pane.path = path;
                        pane.all = list;
                        if moved {
                            pane.selected.clear();
                            pane.anchor = None;
                            pane.query.clear();
                            pane.filter_input
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            pane.scroll.scroll_to_item(0, ScrollStrategy::Top);
                        }
                        pane.apply_filter();
                    }
                    Err(err) => pane.error = Some(SharedString::from(format!("{err:#}"))),
                }
                let shown = pane.path.clone();
                pane.path_input
                    .update(cx, |input, cx| input.set_value(shown, window, cx));
                cx.notify();
            },
        );
        cx.notify();
    }

    fn refresh(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.pane(side).path.clone();
        if !path.is_empty() {
            self.navigate(side, path, window, cx);
        }
    }

    fn go_up(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.pane(side);
        let parent = pane
            .backend
            .as_ref()
            .and_then(|b| b.parent(&pane.path));
        if let Some(parent) = parent {
            self.navigate(side, parent, window, cx);
        }
    }

    fn toggle_hidden(&mut self, side: Side, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.show_hidden = !pane.show_hidden;
        pane.apply_filter();
        cx.notify();
    }

    /// Double-click / Enter: folders open, files go to the other pane.
    fn open_entry(&mut self, side: Side, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.pane(side);
        let (Some(backend), Some(entry)) = (pane.backend.clone(), pane.entries.get(ix).cloned())
        else {
            return;
        };
        let path = backend.join(&pane.path, &entry.name);
        // Remote symlinks don't say what they point at; try them as folders.
        if entry.is_dir || (entry.is_symlink && !backend.is_local()) {
            self.navigate(side, path, window, cx);
        } else {
            self.transfer_items(side, vec![(path, entry)], window, cx);
        }
    }

    // ---- selection & keyboard ---------------------------------------------

    fn click_row(
        &mut self,
        side: Side,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active = side;
        window.focus(&self.focus, cx);
        let mods = event.modifiers();
        let secondary = mods.secondary(cx);
        let pane = self.pane_mut(side);
        let toggle = secondary || pane.multi;
        match pane.anchor {
            Some(anchor) if mods.shift => {
                let range = anchor.min(ix)..=anchor.max(ix);
                if pane.multi {
                    pane.selected.extend(range);
                } else {
                    pane.selected = range.collect();
                }
            }
            _ if toggle => {
                if !pane.selected.remove(&ix) {
                    pane.selected.insert(ix);
                }
                pane.anchor = Some(ix);
            }
            _ => pane.select_only(ix),
        }
        // In checkbox mode the second click of a double click undid the
        // first toggle, so opening leaves the selection as it was.
        if event.click_count() == 2 && !mods.shift && !secondary {
            self.open_entry(side, ix, window, cx);
        }
        cx.notify();
    }

    /// Right-click on a row: menus act on the selection, so make sure the row
    /// is part of it.
    fn row_menu(&mut self, side: Side, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = side;
        self.row_menu = true;
        window.focus(&self.focus, cx);
        let pane = self.pane_mut(side);
        if !pane.selected.contains(&ix) {
            pane.select_only(ix);
        }
        cx.notify();
    }

    /// Right-click on the list background (after any row handler).
    fn list_menu(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        self.active = side;
        if !std::mem::take(&mut self.row_menu) {
            window.focus(&self.focus, cx);
            let pane = self.pane_mut(side);
            pane.selected.clear();
            pane.anchor = None;
        }
        cx.notify();
    }

    /// Select everything, or nothing if everything already is (`toggle`).
    fn select_all(&mut self, side: Side, toggle: bool, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        let all = pane.entries.len();
        pane.selected = if toggle && pane.selected.len() == all {
            BTreeSet::new()
        } else {
            (0..all).collect()
        };
        cx.notify();
    }

    fn toggle_multi(&mut self, side: Side, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.multi = !pane.multi;
        if !pane.multi && pane.selected.len() > 1 {
            pane.selected.clear();
            pane.anchor = None;
        }
        cx.notify();
    }

    fn sort_by(&mut self, side: Side, key: SortKey, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        if pane.sort == key {
            pane.ascending = !pane.ascending;
        } else {
            pane.sort = key;
            // Newest / largest first is what people usually want.
            pane.ascending = key == SortKey::Name;
        }
        pane.apply_filter();
        cx.notify();
    }

    fn move_cursor(&mut self, side: Side, delta: isize, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        if pane.entries.is_empty() {
            return;
        }
        let last = pane.entries.len() as isize - 1;
        let ix = match pane.anchor {
            Some(ix) => (ix as isize + delta).clamp(0, last) as usize,
            None if delta < 0 => last as usize,
            None => 0,
        };
        pane.selected = BTreeSet::from([ix]);
        pane.anchor = Some(ix);
        pane.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        let key = event.keystroke.key.as_str();
        if self.dialog.is_some() {
            // Confirm-only dialogs (delete / replace) keep focus on the manager.
            match key {
                "escape" => self.close_dialog(window, cx),
                "enter" => self.confirm_dialog(window, cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let side = self.active;
        let cmd = event.keystroke.modifiers.secondary(cx);
        match key {
            "up" if cmd => self.go_up(side, window, cx),
            "up" => self.move_cursor(side, -1, cx),
            "down" => self.move_cursor(side, 1, cx),
            "space" => {
                let pane = self.pane_mut(side);
                if let Some(ix) = pane.anchor {
                    pane.multi = true;
                    if !pane.selected.remove(&ix) {
                        pane.selected.insert(ix);
                    }
                    cx.notify();
                }
            }
            "escape" if self.pane(side).multi || !self.pane(side).selected.is_empty() => {
                let pane = self.pane_mut(side);
                pane.multi = false;
                pane.selected.clear();
                pane.anchor = None;
                cx.notify();
            }
            "f2" => self.ask_rename(side, window, cx),
            "tab" => {
                self.active = side.other();
                cx.notify();
            }
            "enter" => {
                let pane = self.pane(side);
                if let (1, Some(ix)) = (pane.selected.len(), pane.anchor) {
                    self.open_entry(side, ix, window, cx);
                }
            }
            "backspace" if cmd => self.ask_delete(side, window, cx),
            "backspace" => self.go_up(side, window, cx),
            "delete" => self.ask_delete(side, window, cx),
            "a" if cmd => self.select_all(side, false, cx),
            "c" if cmd => self.copy_paths(side, true, cx),
            "r" if cmd => self.refresh(side, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---- remote connection ------------------------------------------------

    fn sftp_hosts(&self, cx: &App) -> Vec<SessionProfile> {
        let query = self.host_search.read(cx).value().trim().to_lowercase();
        self.hosts
            .iter()
            .filter(|p| is_file_host(p))
            .filter(|p| {
                query.is_empty()
                    || p.name.to_lowercase().contains(&query)
                    || session_target(p).to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }

    fn connect_from_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.host_search.read(cx).value().trim().to_string();
        let matches = self.sftp_hosts(cx);
        if !query.is_empty() && matches.len() == 1 {
            let profile = matches[0].clone();
            self.connect(profile, window, cx);
            return;
        }
        match parse_quick_connect(&query).filter(is_file_host) {
            Some(profile) => self.connect(profile, window, cx),
            None => {
                self.remote_error = Some(i18n::tr("files.connect_hint"));
                cx.notify();
            }
        }
    }

    pub fn connect(&mut self, profile: SessionProfile, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.status, RemoteStatus::Connecting(_)) {
            return;
        }
        let label = SharedString::from(profile.name.clone());
        let connect = match self.connector(&profile) {
            Ok(connect) => connect,
            Err(err) => {
                self.remote_error = Some(i18n::tr_error("app.connect_failed", &format!("{err:#}")));
                cx.notify();
                return;
            }
        };
        self.remote.reset();
        self.remote_error = None;
        self.status = RemoteStatus::Connecting(label.clone());
        Self::spawn_io(
            window,
            cx,
            async move {
                let backend = connect.await?;
                let home = backend.home().await?;
                Ok((backend, home))
            },
            move |this, result, window, cx| {
                match result {
                    Ok((backend, home)) => {
                        this.status = RemoteStatus::Connected(label);
                        this.remote.backend = Some(backend);
                        this.navigate(Side::Remote, home, window, cx);
                    }
                    Err(err) => {
                        this.status = RemoteStatus::Disconnected;
                        this.remote_error =
                            Some(i18n::tr_error("app.connect_failed", &format!("{err:#}")));
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Resolve `profile` into a future that opens its backend.
    fn connector(
        &self,
        profile: &SessionProfile,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<Backend>> + Send + 'static> {
        enum Target {
            Sftp(Box<gale_ssh::SshSessionConfig>),
            Ftp(Box<FtpClientConfig>),
        }
        let target = if profile.protocol == Protocol::Ftp {
            let prompts = Arc::new(self.prompts.clone());
            Target::Ftp(Box::new(FtpClientConfig::from_profile(profile)?.with_prompt(prompts)))
        } else {
            let store = SessionStore::open_default().ok();
            Target::Sftp(Box::new(ssh_config(profile, store.as_ref(), &self.prompts)?))
        };
        Ok(async move {
            Ok(match target {
                Target::Sftp(config) => Backend::Sftp(Arc::new(SftpClient::connect(*config).await?)),
                Target::Ftp(config) => Backend::Ftp(Arc::new(FtpClient::connect(*config).await?)),
            })
        })
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.remote.reset();
        self.status = RemoteStatus::Disconnected;
        if self.active == Side::Remote {
            self.active = Side::Local;
        }
        cx.notify();
    }

    // ---- transfers --------------------------------------------------------

    /// Copy `items` from `from` into the other pane's folder, asking first if
    /// anything would be replaced.
    fn transfer_items(
        &mut self,
        from: Side,
        items: Vec<(String, FileEntry)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dst = self.pane(from.other());
        if items.is_empty() || dst.backend.is_none() || dst.path.is_empty() {
            return;
        }
        let conflicts: Vec<String> = items
            .iter()
            .filter(|(_, e)| dst.all.iter().any(|d| d.name == e.name))
            .map(|(_, e)| e.name.clone())
            .collect();
        if conflicts.is_empty() {
            for (src, entry) in items {
                self.start_transfer(from, src, entry, window, cx);
            }
        } else {
            self.dialog = Some(Dialog::Overwrite { from, items, conflicts });
            cx.notify();
        }
    }

    fn start_transfer(
        &mut self,
        from: Side,
        src: String,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let to = from.other();
        let (Some(src_backend), Some(dst_backend)) =
            (self.pane(from).backend.clone(), self.pane(to).backend.clone())
        else {
            return;
        };
        let dst_dir = self.pane(to).path.clone();
        let id = self.next_transfer;
        self.next_transfer += 1;
        let done = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        self.transfers.push(Transfer {
            id,
            name: SharedString::from(entry.name.clone()),
            from,
            total: if entry.is_dir { None } else { entry.size },
            done: done.clone(),
            cancel: cancel.clone(),
            state: TransferState::Running,
        });

        let (tx, mut rx) = oneshot::channel();
        let task_dst = dst_dir.clone();
        runtime().spawn(async move {
            let mut progress = move |n: u64| {
                done.fetch_add(n, Ordering::Relaxed);
                !cancel.load(Ordering::Relaxed)
            };
            let result = transfer(
                &src_backend,
                &src,
                &dst_backend,
                &task_dst,
                &entry.name,
                entry.is_dir,
                &mut progress,
            )
            .await;
            let _ = tx.send(result);
        });

        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(PROGRESS_TICK).await;
            let outcome = match rx.try_recv() {
                Ok(result) => Some(result.map(|_| ())),
                Err(oneshot::error::TryRecvError::Empty) => None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    Some(Err(anyhow::anyhow!("transfer task stopped")))
                }
            };
            let finished = outcome.is_some();
            let alive = this
                .update_in(cx, |this, window, cx| {
                    if let Some(result) = outcome {
                        this.finish_transfer(id, to, &dst_dir, result, window, cx);
                    }
                    cx.notify();
                })
                .is_ok();
            if finished || !alive {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    fn finish_transfer(
        &mut self,
        id: usize,
        to: Side,
        dst_dir: &str,
        result: anyhow::Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
            t.state = match result {
                Ok(()) => TransferState::Done,
                Err(err) if is_cancelled(&err) => TransferState::Cancelled,
                Err(err) => TransferState::Failed(SharedString::from(format!("{err:#}"))),
            };
        }
        if self.pane(to).path == dst_dir {
            self.refresh(to, window, cx);
        }
    }

    fn cancel_transfer(&mut self, id: usize) {
        if let Some(t) = self.transfers.iter().find(|t| t.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .retain(|t| matches!(t.state, TransferState::Running));
        cx.notify();
    }

    fn upload_external(&mut self, paths: &[std::path::PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let items = paths
            .iter()
            .filter_map(|path| {
                let meta = std::fs::metadata(path).ok()?;
                let name = path.file_name()?.to_string_lossy().into_owned();
                Some((
                    path.to_string_lossy().into_owned(),
                    FileEntry {
                        name,
                        is_dir: meta.is_dir(),
                        is_symlink: false,
                        size: (!meta.is_dir()).then_some(meta.len()),
                        modified: None,
                        permissions: None,
                    },
                ))
            })
            .collect();
        self.transfer_items(Side::Local, items, window, cx);
    }

    // ---- dialogs ----------------------------------------------------------

    fn open_dialog(&mut self, dialog: Dialog, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = Some(dialog);
        let text = text.to_string();
        self.dialog_input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn ask_new_folder(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane(side).backend.is_some() {
            self.open_dialog(Dialog::NewFolder(side), "", window, cx);
        }
    }

    fn ask_rename(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let selection = self.pane(side).selection();
        if let [(_, entry)] = selection.as_slice() {
            let from = entry.name.clone();
            self.open_dialog(Dialog::Rename { side, from: from.clone() }, &from, window, cx);
        }
    }

    fn ask_delete(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.pane(side).selection();
        if !items.is_empty() {
            self.dialog = Some(Dialog::Delete { side, items });
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn ask_new_file(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane(side).backend.is_some() {
            self.open_dialog(Dialog::NewFile(side), "", window, cx);
        }
    }

    fn ask_chmod(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.pane(side).selection();
        let Some((_, first)) = items.first() else {
            return;
        };
        let mode = first
            .permissions
            .map(|p| format!("{:03o}", p & 0o7777))
            .unwrap_or_default();
        self.open_dialog(Dialog::Chmod { side, items }, &mode, window, cx);
    }

    /// Copy the selected names (or full paths), one per line.
    fn copy_paths(&mut self, side: Side, full: bool, cx: &mut Context<Self>) {
        let text = self
            .pane(side)
            .selection()
            .into_iter()
            .map(|(path, entry)| if full { path } else { entry.name })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn run_menu(&mut self, side: Side, cmd: MenuCmd, window: &mut Window, cx: &mut Context<Self>) {
        let local_paths = || -> Vec<std::path::PathBuf> {
            if side != Side::Local {
                return Vec::new();
            }
            self.pane(side)
                .selection()
                .into_iter()
                .map(|(path, _)| path.into())
                .collect()
        };
        match cmd {
            MenuCmd::Open => {
                if let Some(ix) = self.pane(side).selected.first().copied() {
                    self.open_entry(side, ix, window, cx);
                }
            }
            MenuCmd::Transfer => {
                let items = self.pane(side).selection();
                self.transfer_items(side, items, window, cx);
            }
            MenuCmd::OpenWithSystem => local_paths().iter().for_each(|p| cx.open_with_system(p)),
            MenuCmd::Reveal => {
                if let Some(path) = local_paths().first() {
                    cx.reveal_path(path);
                }
            }
            MenuCmd::CopyName => self.copy_paths(side, false, cx),
            MenuCmd::CopyPath => self.copy_paths(side, true, cx),
            MenuCmd::Rename => self.ask_rename(side, window, cx),
            MenuCmd::Chmod => self.ask_chmod(side, window, cx),
            MenuCmd::Delete => self.ask_delete(side, window, cx),
            MenuCmd::NewFolder => self.ask_new_folder(side, window, cx),
            MenuCmd::NewFile => self.ask_new_file(side, window, cx),
            MenuCmd::SelectAll => self.select_all(side, false, cx),
            MenuCmd::ToggleMulti => self.toggle_multi(side, cx),
            MenuCmd::ToggleHidden => self.toggle_hidden(side, cx),
            MenuCmd::Refresh => self.refresh(side, window, cx),
        }
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let name = self.dialog_input.read(cx).value().trim().to_string();
        let valid_name = !name.is_empty() && !name.contains('/') && name != "." && name != "..";
        let mode = u32::from_str_radix(&name, 8).ok().filter(|m| *m <= 0o7777);
        match dialog {
            Dialog::NewFolder(_) | Dialog::NewFile(_) | Dialog::Rename { .. } if !valid_name => {
                self.dialog = Some(dialog);
                return;
            }
            Dialog::Chmod { .. } if mode.is_none() => {
                self.dialog = Some(dialog);
                return;
            }
            Dialog::NewFolder(side) => {
                let Some(backend) = self.pane(side).backend.clone() else {
                    return self.close_dialog(window, cx);
                };
                let path = backend.join(&self.pane(side).path, &name);
                self.run_mutation(side, async move { backend.mkdir(&path).await }, window, cx);
            }
            Dialog::NewFile(side) => {
                let Some(backend) = self.pane(side).backend.clone() else {
                    return self.close_dialog(window, cx);
                };
                if self.pane(side).all.iter().any(|e| e.name == name) {
                    self.dialog = Some(Dialog::NewFile(side));
                    return;
                }
                let path = backend.join(&self.pane(side).path, &name);
                self.run_mutation(side, async move { backend.create_file(&path).await }, window, cx);
            }
            Dialog::Chmod { side, items } => {
                let Some(backend) = self.pane(side).backend.clone() else {
                    return self.close_dialog(window, cx);
                };
                let mode = mode.unwrap_or_default();
                self.run_mutation(
                    side,
                    async move {
                        for (path, _) in items {
                            backend.chmod(&path, mode).await?;
                        }
                        Ok(())
                    },
                    window,
                    cx,
                );
            }
            Dialog::Rename { side, from } => {
                let Some(backend) = self.pane(side).backend.clone() else {
                    return self.close_dialog(window, cx);
                };
                let dir = self.pane(side).path.clone();
                let (from, to) = (backend.join(&dir, &from), backend.join(&dir, &name));
                self.run_mutation(side, async move { backend.rename(&from, &to).await }, window, cx);
            }
            Dialog::Delete { side, items } => {
                let Some(backend) = self.pane(side).backend.clone() else {
                    return self.close_dialog(window, cx);
                };
                self.run_mutation(
                    side,
                    async move {
                        for (path, entry) in items {
                            backend.remove(&path, entry.is_dir).await?;
                        }
                        Ok(())
                    },
                    window,
                    cx,
                );
            }
            Dialog::Overwrite { from, items, .. } => {
                for (src, entry) in items {
                    self.start_transfer(from, src, entry, window, cx);
                }
            }
        }
        self.close_dialog(window, cx);
    }

    /// Run a change on `side`, then reload it (showing any error).
    fn run_mutation(
        &mut self,
        side: Side,
        fut: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Self::spawn_io(window, cx, fut, move |this, result, window, cx| {
            this.refresh(side, window, cx);
            if let Err(err) = result {
                this.show_error(side, err, cx);
            }
        });
    }
}

fn is_file_host(p: &SessionProfile) -> bool {
    match p.protocol {
        Protocol::Ssh | Protocol::Sftp => p.ssh.is_some(),
        Protocol::Ftp => p.ftp.is_some(),
        _ => false,
    }
}

fn is_cancelled(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<gale_ssh::Error>(), Some(gale_ssh::Error::Cancelled))
        || matches!(err.downcast_ref::<gale_ftp::Error>(), Some(gale_ftp::Error::Cancelled))
}

// ---- rendering ------------------------------------------------------------

fn checkbox_icon(checked: bool, partial: bool, theme: &Theme) -> Icon {
    let (icon, color) = match (checked, partial) {
        (true, _) => (Icon::new(assets::IconName::SquareCheck), theme.primary),
        (false, true) => (Icon::new(IconName::Minus), theme.primary),
        (false, false) => (Icon::new(IconName::Square), theme.muted_foreground),
    };
    icon.small().text_color(color)
}

fn format_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

impl Render for FileManager {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let remote = if self.remote.backend.is_some() {
            self.render_pane(Side::Remote, window, cx).into_any_element()
        } else {
            self.render_picker(cx).into_any_element()
        };

        div()
            .id("file-manager")
            .key_context("FileManager")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .bg(theme.background)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_pane(Side::Local, window, cx))
                    .child(div().w(px(1.)).h_full().bg(theme.border))
                    .child(remote),
            )
            .children(self.render_transfers(cx))
            .children(self.render_dialog(cx))
    }
}

impl FileManager {
    fn pane_header(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (icon, title) = match (side, &self.status) {
            (Side::Local, _) => (IconName::HardDrive, i18n::tr("files.local")),
            (Side::Remote, RemoteStatus::Connected(label)) => (IconName::Globe, label.clone()),
            (Side::Remote, _) => (IconName::Globe, i18n::tr("files.remote")),
        };
        let active = self.active == side;
        div()
            .h(px(40.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(Icon::new(icon).small().text_color(if active {
                theme.primary
            } else {
                theme.muted_foreground
            }))
            .child(Label::new(title).text_sm().font_weight(FontWeight::SEMIBOLD))
            .when(
                side == Side::Remote
                    && self.remote.backend.as_ref().is_some_and(Backend::is_insecure),
                |s| {
                    s.child(
                        div()
                            .px_1p5()
                            .rounded(px(4.))
                            .bg(theme.warning.opacity(0.15))
                            .text_xs()
                            .text_color(theme.warning)
                            .child(i18n::tr("files.insecure")),
                    )
                },
            )
            .child(div().flex_1())
            .when(side == Side::Remote && self.remote.backend.is_some(), |s| {
                s.child(
                    Button::new("sftp-disconnect")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .label(i18n::tr("files.disconnect"))
                        .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
                )
            })
    }

    fn render_pane(&self, side: Side, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pane = self.pane(side);
        let key = side.key();
        let id = |name: &str| SharedString::from(format!("{key}-{name}"));
        let selected = pane.selected.len();
        let can_transfer = selected > 0 && self.pane(side.other()).backend.is_some();

        let toolbar = div()
            .px_2()
            .pb_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                Button::new(id("up"))
                    .ghost()
                    .small()
                    .icon(IconName::ArrowUp)
                    .on_click(cx.listener(move |this, _, window, cx| this.go_up(side, window, cx))),
            )
            .child(div().flex_1().min_w_0().child(Input::new(&pane.path_input).small()))
            .child(
                div().w(px(140.)).flex_none().child(
                    Input::new(&pane.filter_input)
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).xsmall()),
                ),
            )
            .child(
                Button::new(id("multi"))
                    .ghost()
                    .small()
                    .icon(Icon::new(assets::IconName::ListChecks))
                    .selected(pane.multi)
                    .tooltip(i18n::tr("files.multi_select"))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_multi(side, cx))),
            )
            .child(
                Button::new(id("refresh"))
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .on_click(cx.listener(move |this, _, window, cx| this.refresh(side, window, cx))),
            )
            .child(
                Button::new(id("mkdir"))
                    .ghost()
                    .small()
                    .icon(Icon::new(assets::IconName::FolderPlus))
                    .tooltip(i18n::tr("files.new_folder"))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.ask_new_folder(side, window, cx)),
                    ),
            )
            .child(
                Button::new(id("hidden"))
                    .ghost()
                    .small()
                    .icon(if pane.show_hidden { IconName::Eye } else { IconName::EyeOff })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_hidden(side, cx))),
            );

        let sort_header = |key: SortKey, label: &str| {
            let active = pane.sort == key;
            div()
                .id(id(&format!("sort-{label}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_0p5()
                .cursor_pointer()
                .hover(|s| s.text_color(theme.foreground))
                .when(active, |s| s.text_color(theme.foreground))
                .when(key == SortKey::Size, |s| s.justify_end())
                .child(i18n::tr(label))
                .when(active, |s| {
                    s.child(
                        Icon::new(if pane.ascending {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall(),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.sort_by(side, key, cx)))
        };
        let all_selected = selected > 0 && selected == pane.entries.len();
        let columns = div()
            .h(px(24.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .border_b_1()
            .border_color(theme.border)
            .when(pane.multi, |s| {
                s.child(
                    div()
                        .id(id("check-all"))
                        .w(px(16.))
                        .flex_none()
                        .cursor_pointer()
                        .child(checkbox_icon(all_selected, selected > 0, &theme))
                        .on_click(cx.listener(move |this, _, _, cx| this.select_all(side, true, cx))),
                )
            })
            .child(div().w(px(16.)).flex_none())
            .child(div().flex_1().child(sort_header(SortKey::Name, "files.col_name")))
            .child(div().w(px(120.)).child(sort_header(SortKey::Modified, "files.col_modified")))
            .child(div().w(px(76.)).child(sort_header(SortKey::Size, "files.col_size")))
            .child(div().w(px(84.)).child(i18n::tr("files.col_mode")));

        let body = if let Some(err) = &pane.error {
            div()
                .p_3()
                .child(Label::new(err.clone()).text_sm().text_color(theme.danger))
                .into_any_element()
        } else if pane.entries.is_empty() {
            div()
                .p_4()
                .flex()
                .justify_center()
                .child(
                    Label::new(if pane.loading {
                        i18n::tr("files.loading")
                    } else {
                        i18n::tr("files.empty_folder")
                    })
                    .text_sm()
                    .text_color(theme.muted_foreground),
                )
                .into_any_element()
        } else {
            uniform_list(
                id("list"),
                pane.entries.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    this.render_rows(side, range, cx)
                }),
            )
            .track_scroll(&pane.scroll)
            .size_full()
            .into_any_element()
        };

        let manager = cx.entity().downgrade();
        let list = div()
            .id(id("drop"))
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(transparent_black())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.list_menu(side, window, cx)),
            )
            .on_drop(cx.listener(move |this, dragged: &DraggedFiles, window, cx| {
                if dragged.from != side {
                    this.transfer_items(dragged.from, dragged.items.clone(), window, cx);
                }
            }))
            .drag_over::<DraggedFiles>(move |style, dragged, _, cx| {
                if dragged.from != side {
                    style.border_color(cx.theme().primary).bg(cx.theme().accent)
                } else {
                    style
                }
            })
            .when(side == Side::Remote, |s| {
                s.on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                    this.upload_external(paths.paths(), window, cx);
                }))
                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                    style.border_color(cx.theme().primary).bg(cx.theme().accent)
                })
            })
            .child(body)
            .context_menu(move |menu, _, cx| Self::build_menu(manager.clone(), side, menu, cx));

        let footer = div()
            .h(px(36.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Label::new(if selected > 0 {
                    i18n::tr_selected(selected, pane.entries.len())
                } else {
                    i18n::tr_count("files.items", pane.entries.len())
                })
                .text_xs()
                .text_color(theme.muted_foreground),
            )
            .child(div().flex_1())
            .child(
                Button::new(id("rename"))
                    .ghost()
                    .xsmall()
                    .label(i18n::tr("files.rename"))
                    .disabled(selected != 1)
                    .on_click(cx.listener(move |this, _, window, cx| this.ask_rename(side, window, cx))),
            )
            .child(
                Button::new(id("delete"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Delete)
                    .label(i18n::tr("files.delete"))
                    .disabled(selected == 0)
                    .on_click(cx.listener(move |this, _, window, cx| this.ask_delete(side, window, cx))),
            )
            .child(
                Button::new(id("transfer"))
                    .primary()
                    .xsmall()
                    .icon(if side == Side::Local {
                        IconName::ArrowRight
                    } else {
                        IconName::ArrowLeft
                    })
                    .label(if side == Side::Local {
                        i18n::tr("files.upload")
                    } else {
                        i18n::tr("files.download")
                    })
                    .disabled(!can_transfer)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let items = this.pane(side).selection();
                        this.transfer_items(side, items, window, cx);
                    })),
            );

        div()
            .id(id("pane"))
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.active != side {
                        this.active = side;
                        cx.notify();
                    }
                }),
            )
            .child(self.pane_header(side, cx))
            .child(toolbar)
            .child(columns)
            .child(list)
            .child(footer)
    }

    /// Right-click menu for `side`, built from the selection at open time.
    fn build_menu(
        manager: WeakEntity<Self>,
        side: Side,
        menu: PopupMenu,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(entity) = manager.upgrade() else {
            return menu;
        };
        let this = entity.read(cx);
        let pane = this.pane(side);
        let Some(backend) = pane.backend.as_ref() else {
            return menu;
        };
        let selection = pane.selection();
        let count = selection.len();
        let local = backend.is_local();
        let opens_as_folder = match selection.as_slice() {
            [(_, e)] => e.is_dir || (e.is_symlink && !local),
            _ => false,
        };
        let other_ready = this.pane(side.other()).backend.is_some();
        let (multi, hidden) = (pane.multi, pane.show_hidden);

        let item = |label: &str, icon: Option<Icon>, cmd: MenuCmd| {
            let manager = manager.clone();
            PopupMenuItem::new(i18n::tr(label))
                .when_some(icon, |item, icon| item.icon(icon))
                .on_click(move |_, window, cx| {
                    let _ = manager.update(cx, |this, cx| this.run_menu(side, cmd, window, cx));
                })
        };

        let mut menu = menu.min_w(px(200.));
        if count > 0 {
            if opens_as_folder {
                menu = menu.item(item("files.open", Some(IconName::FolderOpen.into()), MenuCmd::Open));
            }
            if other_ready {
                let (label, icon) = if local {
                    ("files.upload", assets::IconName::Upload)
                } else {
                    ("files.download", assets::IconName::Download)
                };
                menu = menu.item(item(label, Some(Icon::new(icon)), MenuCmd::Transfer));
            }
            if local {
                menu = menu
                    .item(item(
                        "files.open_with_system",
                        Some(IconName::ExternalLink.into()),
                        MenuCmd::OpenWithSystem,
                    ))
                    .item(item("files.reveal", None, MenuCmd::Reveal));
            }
            menu = menu
                .separator()
                .item(item("files.copy_name", Some(IconName::Copy.into()), MenuCmd::CopyName))
                .item(item(
                    "files.copy_path",
                    Some(Icon::new(assets::IconName::ClipboardCopy)),
                    MenuCmd::CopyPath,
                ))
                .separator()
                .item(
                    item("files.rename", Some(Icon::new(assets::IconName::Pencil)), MenuCmd::Rename)
                        .disabled(count != 1),
                )
                .item(item("files.chmod", Some(Icon::new(assets::IconName::KeyRound)), MenuCmd::Chmod))
                .item(item("files.delete", Some(IconName::Delete.into()), MenuCmd::Delete))
                .separator();
        }
        menu.item(item(
            "files.new_folder",
            Some(Icon::new(assets::IconName::FolderPlus)),
            MenuCmd::NewFolder,
        ))
        .item(item("files.new_file", Some(Icon::new(assets::IconName::FilePlus)), MenuCmd::NewFile))
        .separator()
        .item(item("files.select_all", None, MenuCmd::SelectAll))
        .item(item("files.multi_select", None, MenuCmd::ToggleMulti).checked(multi))
        .item(item("files.show_hidden", None, MenuCmd::ToggleHidden).checked(hidden))
        .item(item("files.refresh", Some(IconName::RefreshCw.into()), MenuCmd::Refresh))
    }

    fn render_rows(&self, side: Side, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let pane = self.pane(side);
        let Some(backend) = pane.backend.as_ref() else {
            return Vec::new();
        };
        let focused_side = self.active == side;
        range
            .filter_map(|ix| pane.entries.get(ix).map(|e| (ix, e)))
            .map(|(ix, entry)| {
                let selected = pane.selected.contains(&ix);
                let items = if selected {
                    pane.selection()
                } else {
                    vec![(backend.join(&pane.path, &entry.name), entry.clone())]
                };
                let preview = DraggedFiles { from: side, items };
                let folder_like = entry.is_dir || (entry.is_symlink && !backend.is_local());
                div()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_H))
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .when(selected, |s| {
                        s.bg(if focused_side { theme.list_active } else { theme.secondary })
                    })
                    .when(!selected, |s| s.hover(|s| s.bg(theme.list_hover)))
                    .when(pane.multi, |s| {
                        s.child(
                            div()
                                .w(px(16.))
                                .flex_none()
                                .child(checkbox_icon(selected, false, &theme)),
                        )
                    })
                    .child(
                        img(file_icons::icon_path(&entry.name, folder_like))
                            .size(px(16.))
                            .flex_none()
                            .when(entry.is_hidden(), |s| s.opacity(0.6)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when(entry.is_hidden(), |s| s.text_color(theme.muted_foreground))
                            .child(entry.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(entry.modified.map(format_time).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(76.))
                            .text_right()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(entry.size.map(format_size).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(84.))
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(
                                entry
                                    .permissions
                                    .map(|p| format_mode(p, entry.is_dir, entry.is_symlink))
                                    .unwrap_or_default(),
                            ),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.click_row(side, ix, event, window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| this.row_menu(side, ix, window, cx)),
                    )
                    .on_drag(preview, |dragged, _, _, cx| {
                        let (_, first) = &dragged.items[0];
                        let label = match dragged.items.len() {
                            1 => first.name.clone(),
                            n => format!("{} +{}", first.name, n - 1),
                        };
                        let icon = file_icons::icon_path(&first.name, first.is_dir);
                        cx.new(|_| DragPreview {
                            label: label.into(),
                            icon,
                        })
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let hosts = self.sftp_hosts(cx);
        let connecting = match &self.status {
            RemoteStatus::Connecting(label) => Some(label.clone()),
            _ => None,
        };

        let host_rows = hosts.into_iter().map(|profile| {
            let subtitle = format!("{} · {}", profile.protocol.as_str(), session_target(&profile));
            div()
                .id(SharedString::from(format!("sftp-host-{}", profile.id)))
                .h(px(48.))
                .px_3()
                .rounded(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .bg(theme.secondary)
                .hover(|s| s.bg(theme.secondary_hover))
                .cursor_pointer()
                .child(
                    Icon::new(if profile.protocol == Protocol::Ftp {
                        IconName::Folder
                    } else {
                        IconName::Globe
                    })
                    .small()
                    .text_color(theme.primary),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(Label::new(profile.name.clone()).text_sm().font_weight(FontWeight::SEMIBOLD))
                        .child(Label::new(subtitle).text_xs().text_color(theme.muted_foreground)),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.connect(profile.clone(), window, cx);
                }))
        })
        .collect::<Vec<_>>();

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.pane_header(Side::Remote, cx))
            .child(
                div()
                    .id("sftp-picker")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        Label::new(i18n::tr("files.pick_host"))
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(i18n::tr("files.pick_host_hint"))
                            .text_sm()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.host_search)
                                        .cleanable(true)
                                        .prefix(Icon::new(IconName::Search).small()),
                                ),
                            )
                            .child(
                                Button::new("sftp-connect")
                                    .primary()
                                    .label(i18n::tr("app.connect"))
                                    .loading(connecting.is_some())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.connect_from_search(window, cx);
                                    })),
                            ),
                    )
                    .children(connecting.map(|label| {
                        Label::new(format!("{} {label}…", i18n::tr("files.connecting")))
                            .text_sm()
                            .text_color(theme.muted_foreground)
                    }))
                    .children(self.remote_error.clone().map(|err| {
                        Label::new(err).text_sm().text_color(theme.danger)
                    }))
                    .children(host_rows),
            )
    }

    fn render_transfers(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.transfers.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let running = self
            .transfers
            .iter()
            .filter(|t| matches!(t.state, TransferState::Running))
            .count();

        let rows = self.transfers.iter().rev().map(|t| {
            let done = t.done.load(Ordering::Relaxed);
            let fraction = match (&t.state, t.total) {
                (TransferState::Done, _) => 1.,
                (_, Some(total)) if total > 0 => (done as f32 / total as f32).min(1.),
                _ => 0.,
            };
            let (status, color) = match &t.state {
                TransferState::Running => (
                    match t.total {
                        Some(total) => format!("{} / {}", format_size(done), format_size(total)),
                        None => format_size(done),
                    },
                    theme.muted_foreground,
                ),
                TransferState::Done => (i18n::tr("files.done").to_string(), theme.success),
                TransferState::Cancelled => {
                    (i18n::tr("files.cancelled").to_string(), theme.muted_foreground)
                }
                TransferState::Failed(err) => (err.to_string(), theme.danger),
            };
            let id = t.id;
            div()
                .h(px(26.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_xs()
                .child(
                    Icon::new(if t.from == Side::Local {
                        IconName::ArrowUp
                    } else {
                        IconName::ArrowDown
                    })
                    .xsmall()
                    .text_color(theme.primary),
                )
                .child(
                    div()
                        .w(px(220.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(t.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .h(px(4.))
                        .rounded_full()
                        .bg(theme.secondary)
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .w(relative(fraction))
                                .bg(if matches!(t.state, TransferState::Failed(_)) {
                                    theme.danger
                                } else {
                                    theme.primary
                                }),
                        ),
                )
                .child(
                    div()
                        .w(px(220.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(color)
                        .child(status),
                )
                .child(div().w(px(24.)).when(matches!(t.state, TransferState::Running), |s| {
                    s.child(
                        Button::new(SharedString::from(format!("transfer-cancel-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, _, _| this.cancel_transfer(id))),
                    )
                }))
        });

        Some(
            div()
                .border_t_1()
                .border_color(theme.border)
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            Label::new(i18n::tr("files.transfers"))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .when(running > 0, |s| {
                            s.child(
                                Label::new(running.to_string())
                                    .text_xs()
                                    .text_color(theme.muted_foreground),
                            )
                        })
                        .child(div().flex_1())
                        .child(
                            Button::new("transfers-clear")
                                .ghost()
                                .xsmall()
                                .label(i18n::tr("files.clear_finished"))
                                .disabled(running == self.transfers.len())
                                .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                        ),
                )
                .child(
                    div()
                        .id("transfers-list")
                        .max_h(px(110.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    fn render_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let theme = cx.theme().clone();

        let names = |items: Vec<&str>| {
            let more = items.len().saturating_sub(5);
            div()
                .p_3()
                .rounded(px(8.))
                .bg(theme.secondary)
                .flex()
                .flex_col()
                .gap_1()
                .text_sm()
                .children(items.into_iter().take(5).map(|n| n.to_string()))
                .when(more > 0, |s| {
                    s.child(
                        Label::new(format!("+{more}"))
                            .text_xs()
                            .text_color(theme.muted_foreground),
                    )
                })
        };

        let (title, body, confirm, danger) = match dialog {
            Dialog::NewFolder(_) => (
                i18n::tr("files.new_folder"),
                Input::new(&self.dialog_input).into_any_element(),
                i18n::tr("files.create"),
                false,
            ),
            Dialog::NewFile(_) => (
                i18n::tr("files.new_file"),
                Input::new(&self.dialog_input).into_any_element(),
                i18n::tr("files.create"),
                false,
            ),
            Dialog::Rename { .. } => (
                i18n::tr("files.rename"),
                Input::new(&self.dialog_input).into_any_element(),
                i18n::tr("files.rename"),
                false,
            ),
            Dialog::Chmod { items, .. } => (
                i18n::tr_count("files.chmod_title", items.len()),
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        Label::new(i18n::tr("files.chmod_hint"))
                            .text_sm()
                            .text_color(theme.muted_foreground),
                    )
                    .child(Input::new(&self.dialog_input))
                    .child(names(items.iter().map(|(_, e)| e.name.as_str()).collect()))
                    .into_any_element(),
                i18n::tr("files.apply"),
                false,
            ),
            Dialog::Delete { items, .. } => (
                i18n::tr_count("files.delete_title", items.len()),
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        Label::new(i18n::tr("files.delete_hint"))
                            .text_sm()
                            .text_color(theme.muted_foreground),
                    )
                    .child(names(items.iter().map(|(_, e)| e.name.as_str()).collect()))
                    .into_any_element(),
                i18n::tr("files.delete"),
                true,
            ),
            Dialog::Overwrite { conflicts, .. } => (
                i18n::tr_count("files.overwrite_title", conflicts.len()),
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        Label::new(i18n::tr("files.overwrite_hint"))
                            .text_sm()
                            .text_color(theme.muted_foreground),
                    )
                    .child(names(conflicts.iter().map(String::as_str).collect()))
                    .into_any_element(),
                i18n::tr("files.replace"),
                true,
            ),
        };

        let card = div()
            .w(px(420.))
            .p_5()
            .rounded(px(14.))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_4()
            .child(Label::new(title).text_base().font_weight(FontWeight::SEMIBOLD))
            .child(body)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("files-dialog-cancel")
                            .ghost()
                            .label(i18n::tr("app.cancel"))
                            .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx))),
                    )
                    .child(
                        Button::new("files-dialog-ok")
                            .when(danger, |b| b.danger())
                            .when(!danger, |b| b.primary())
                            .label(confirm)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_dialog(window, cx)),
                            ),
                    ),
            );

        Some(
            div()
                .id("files-dialog")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .bg(hsla(0., 0., 0., 0.45))
                .flex()
                .items_center()
                .justify_center()
                .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                    this.close_dialog(window, cx);
                }))
                .child(card)
                .into_any_element(),
        )
    }
}
