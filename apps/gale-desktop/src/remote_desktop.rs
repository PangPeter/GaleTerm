//! VNC / RDP sessions: network task ↔ [`DesktopPane`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context as _;
use gale_core::vault::CredentialKey;
use gale_core::{Protocol, SessionId, SessionProfile, SessionStore};
use gale_desktop_view::{DesktopInput, DesktopPane, DesktopPaneEvent, FrameBuffer, SoftCursor};
use gale_ftp::{CertDecision, CertInfo, FtpPrompt};
use gale_rdp::{
    RdpCertDecision, RdpCertInfo, RdpClientConfig, RdpInput, RdpOutput, RdpPrompt, RdpSession,
};
use gale_ssh::{open_local_tunnel, LocalTunnel};
use gale_vnc::{VncClientConfig, VncInput, VncOutput, VncPrompt, VncSession};
use gpui_kit::*;
use tokio::sync::mpsc;

fn write_clipboard(cx: &mut App, text: String) {
    if text.is_empty() || text.starts_with("error:") {
        return;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

fn write_clipboard_png(cx: &mut App, png: Vec<u8>) {
    if png.is_empty() {
        return;
    }
    let image = Image::from_bytes(ImageFormat::Png, png);
    cx.write_to_clipboard(ClipboardItem::new_image(&image));
}

use crate::remote_session::{runtime, ssh_config, GuiPrompts};

impl VncPrompt for GuiPrompts {
    fn ask_password(&self, prompt: &str) -> Option<String> {
        self.ask(prompt, None, false).map(|a| a.value)
    }

    fn ask_credential(&self, key: &CredentialKey, prompt: &str) -> Option<String> {
        self.ask_and_track(key, prompt)
    }

    fn saved_credential(&self, key: &CredentialKey) -> Option<String> {
        self.saved(key)
    }

    fn credential_result(&self, key: &CredentialKey, secret: &str, accepted: bool) {
        self.record(key, secret, accepted);
    }
}

impl RdpPrompt for GuiPrompts {
    fn ask_password(&self, prompt: &str) -> Option<String> {
        self.ask(prompt, None, false).map(|a| a.value)
    }

    fn ask_credential(&self, key: &CredentialKey, prompt: &str) -> Option<String> {
        self.ask_and_track(key, prompt)
    }

    fn saved_credential(&self, key: &CredentialKey) -> Option<String> {
        self.saved(key)
    }

    fn credential_result(&self, key: &CredentialKey, secret: &str, accepted: bool) {
        self.record(key, secret, accepted);
    }

    fn confirm_certificate(&self, info: &RdpCertInfo) -> RdpCertDecision {
        // Reuse the FTPS certificate dialog.
        match FtpPrompt::confirm_certificate(
            self,
            &CertInfo {
                host: info.host.clone(),
                port: info.port,
                fingerprint: info.fingerprint.clone(),
                reason: info.reason.clone(),
            },
        ) {
            CertDecision::Reject => RdpCertDecision::Reject,
            CertDecision::AcceptOnce => RdpCertDecision::AcceptOnce,
            CertDecision::AcceptAndSave => RdpCertDecision::AcceptAndSave,
        }
    }
}

enum DesktopBackend {
    Vnc(mpsc::UnboundedSender<VncInput>),
    Rdp(mpsc::UnboundedSender<RdpInput>),
}

impl DesktopBackend {
    fn close(&self) {
        match self {
            Self::Vnc(tx) => {
                let _ = tx.send(VncInput::Close);
            }
            Self::Rdp(tx) => {
                let _ = tx.send(RdpInput::Close);
            }
        }
    }
}

pub struct RemoteDesktopTab {
    pub pane: Entity<DesktopPane>,
    profile: SessionProfile,
    backend: DesktopBackend,
    /// Cleared before reconnect so a dying session cannot mark the tab closed.
    session_alive: Arc<AtomicBool>,
    /// Forwards pane input to the live session; replaced on reconnect.
    _input_sub: Subscription,
}

impl RemoteDesktopTab {
    pub fn connect(
        profile: SessionProfile,
        prompts: GuiPrompts,
        on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<Self> {
        match profile.protocol {
            Protocol::Vnc | Protocol::Rdp => {}
            other => anyhow::bail!("not a remote-desktop protocol: {}", other.as_str()),
        }
        let pane_id = format!(
            "{}-pane-{}",
            profile.protocol.as_str(),
            profile.id.as_str()
        );
        let pane = cx.new(|cx| DesktopPane::new(pane_id, cx));
        let session_alive = Arc::new(AtomicBool::new(true));
        let (backend, input_sub) = start_session(
            &profile,
            pane.clone(),
            prompts,
            session_alive.clone(),
            on_closed,
            window,
            cx,
        )?;
        Ok(Self {
            pane,
            profile,
            backend,
            session_alive,
            _input_sub: input_sub,
        })
    }

    /// Tear down the current session (if any) and dial again into the same pane.
    pub fn reconnect(
        &mut self,
        prompts: GuiPrompts,
        on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<()> {
        self.session_alive.store(false, Ordering::SeqCst);
        self.backend.close();
        self.pane.update(cx, |pane, cx| pane.begin_reconnect(cx));
        let session_alive = Arc::new(AtomicBool::new(true));
        let (backend, input_sub) = start_session(
            &self.profile,
            self.pane.clone(),
            prompts,
            session_alive.clone(),
            on_closed,
            window,
            cx,
        )?;
        self.backend = backend;
        self.session_alive = session_alive;
        self._input_sub = input_sub;
        Ok(())
    }
}

impl Drop for RemoteDesktopTab {
    fn drop(&mut self) {
        self.backend.close();
    }
}

fn start_session(
    profile: &SessionProfile,
    pane: Entity<DesktopPane>,
    prompts: GuiPrompts,
    session_alive: Arc<AtomicBool>,
    on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<(DesktopBackend, Subscription)> {
    match profile.protocol {
        Protocol::Vnc => start_vnc(profile, pane, prompts, session_alive, on_closed, window, cx),
        Protocol::Rdp => start_rdp(profile, pane, prompts, session_alive, on_closed, window, cx),
        other => anyhow::bail!("not a remote-desktop protocol: {}", other.as_str()),
    }
}

fn start_vnc(
    profile: &SessionProfile,
    pane: Entity<DesktopPane>,
    prompts: GuiPrompts,
    session_alive: Arc<AtomicBool>,
    on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<(DesktopBackend, Subscription)> {
    let store = SessionStore::open_default().ok();
    let mut config = VncClientConfig::from_profile(profile)?;
    let jump = profile
        .vnc
        .as_ref()
        .and_then(|v| v.jump_session_id.clone());
    let dest_host = config.host.clone();
    let dest_port = config.port;
    config = config.with_prompt(Arc::new(prompts.clone()));

    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<VncOutput>();
    let label = profile.name.clone();

    runtime().spawn(async move {
        // Tunnel must live on this task (not the UI thread) so SSH password /
        // host-key prompts can still reach the UI event loop.
        let result = async {
            let _tunnel = open_jump_tunnel(
                jump.as_ref(),
                store.as_ref(),
                &prompts,
                &dest_host,
                dest_port,
            )
            .await?;
            let config = if let Some(ref tunnel) = _tunnel {
                config.with_dial("127.0.0.1", tunnel.local_port())
            } else {
                config
            };
            let session = VncSession::connect(config)
                .await
                .with_context(|| format!("connecting to {label}"))?;
            session.run(input_rx, output_tx.clone()).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(err) = result {
            let _ = output_tx.send(VncOutput::CutText(format!("error:{err:#}")));
        }
    });

    let pane_input = input_tx.clone();
    let input_sub = cx.subscribe(&pane, move |_, ev: &DesktopPaneEvent, _| {
        let DesktopPaneEvent::Input(input) = ev else {
            return;
        };
        match input {
                DesktopInput::CutText(text) => {
                    // Update remote cut buffer, then Ctrl+V so the focused app pastes.
                    let _ = pane_input.send(VncInput::CutText(text.clone()));
                    for (down, keysym) in [
                        (true, 0xffe3_u32), // Control_L
                        (true, 0x0076),     // v
                        (false, 0x0076),
                        (false, 0xffe3),
                    ] {
                        let _ = pane_input.send(VncInput::Key { down, keysym });
                    }
                }
                DesktopInput::CutImage(_) => {
                    // VNC RFB has no standard image clipboard; ignore.
                }
                other => {
                    let _ = pane_input.send(to_vnc_input(other));
                }
        }
    });

    let pane_task = pane.downgrade();
    window
        .spawn(cx, async move |cx| {
            let mut last_error = None::<String>;
            while let Some(msg) = output_rx.recv().await {
                if !session_alive.load(Ordering::SeqCst) {
                    return;
                }
                let alive = pane_task.update(cx, |pane, cx| match msg {
                    VncOutput::Ready {
                        width,
                        height,
                        name,
                    } => pane.set_ready(width, height, name, cx),
                    VncOutput::Frame(frame) => {
                        pane.set_frame(vnc_frame(frame), cx);
                    }
                    VncOutput::Cursor(shape) => {
                        pane.set_soft_cursor(
                            shape.map(|c| SoftCursor {
                                hotspot_x: c.hotspot_x,
                                hotspot_y: c.hotspot_y,
                                width: c.width,
                                height: c.height,
                                pixels: c.pixels.into(),
                            }),
                            cx,
                        );
                    }
                    VncOutput::Bell => {}
                    VncOutput::CutText(text) if text.starts_with("error:") => {
                        let reason = text.trim_start_matches("error:").to_string();
                        last_error = Some(reason.clone());
                        pane.set_disconnected(reason, cx);
                    }
                    VncOutput::CutText(text) => write_clipboard(cx, text),
                });
                if alive.is_err() {
                    return;
                }
            }
            if !session_alive.load(Ordering::SeqCst) {
                return;
            }
            let _ = pane_task.update(cx, |pane, cx| {
                if !pane.is_disconnected() {
                    pane.set_disconnected(
                        last_error.unwrap_or_else(|| "Session closed".into()),
                        cx,
                    );
                }
            });
            let _ = cx.update(|window, cx| on_closed(window, cx));
        })
        .detach();

    Ok((DesktopBackend::Vnc(input_tx), input_sub))
}

fn start_rdp(
    profile: &SessionProfile,
    pane: Entity<DesktopPane>,
    prompts: GuiPrompts,
    session_alive: Arc<AtomicBool>,
    on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<(DesktopBackend, Subscription)> {
    let store = SessionStore::open_default().ok();
    let mut config = RdpClientConfig::from_profile(profile)?;
    let jump = profile
        .rdp
        .as_ref()
        .and_then(|v| v.jump_session_id.clone());
    let dest_host = config.host.clone();
    let dest_port = config.port;
    config = config.with_prompt(Arc::new(prompts.clone()));

    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<RdpOutput>();
    let label = profile.name.clone();

    runtime().spawn(async move {
        let tunnel = match open_jump_tunnel(
            jump.as_ref(),
            store.as_ref(),
            &prompts,
            &dest_host,
            dest_port,
        )
        .await
        {
            Ok(t) => t,
            Err(err) => {
                let _ = output_tx.send(RdpOutput::CutText(format!("error:{err:#}")));
                return;
            }
        };
        let config = if let Some(ref tunnel) = tunnel {
            config.with_dial("127.0.0.1", tunnel.local_port())
        } else {
            config
        };

        let out = output_tx.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Keep `_tunnel` moved into this closure so it outlives IronRDP.
            let _tunnel = tunnel;
            let session =
                RdpSession::connect(config).with_context(|| format!("connecting to {label}"))?;
            session.run(input_rx, out.clone())?;
            Ok::<(), anyhow::Error>(())
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = output_tx.send(RdpOutput::CutText(format!("error:{err:#}")));
            }
            Err(err) => {
                let _ = output_tx.send(RdpOutput::CutText(format!("error:{err}")));
            }
        }
    });

    let pane_input = input_tx.clone();
    let input_sub = cx.subscribe(&pane, move |_, ev: &DesktopPaneEvent, _| {
        let DesktopPaneEvent::Input(input) = ev else {
            return;
        };
        let _ = pane_input.send(to_rdp_input(input));
    });

    let pane_task = pane.downgrade();
    window
        .spawn(cx, async move |cx| {
            let mut last_error = None::<String>;
            while let Some(msg) = output_rx.recv().await {
                if !session_alive.load(Ordering::SeqCst) {
                    return;
                }
                let alive = pane_task.update(cx, |pane, cx| match msg {
                    RdpOutput::Ready {
                        width,
                        height,
                        name,
                    } => pane.set_ready(width, height, name, cx),
                    RdpOutput::Frame(frame) => pane.set_frame(rdp_frame(frame), cx),
                    RdpOutput::CutText(text) if text.starts_with("error:") => {
                        let reason = text.trim_start_matches("error:").to_string();
                        last_error = Some(reason.clone());
                        pane.set_disconnected(reason, cx);
                    }
                    RdpOutput::CutText(text) => write_clipboard(cx, text),
                    RdpOutput::CutImage(png) => write_clipboard_png(cx, png),
                });
                if alive.is_err() {
                    return;
                }
            }
            if !session_alive.load(Ordering::SeqCst) {
                return;
            }
            let _ = pane_task.update(cx, |pane, cx| {
                if !pane.is_disconnected() {
                    pane.set_disconnected(
                        last_error.unwrap_or_else(|| "Session closed".into()),
                        cx,
                    );
                }
            });
            let _ = cx.update(|window, cx| on_closed(window, cx));
        })
        .detach();

    Ok((DesktopBackend::Rdp(input_tx), input_sub))
}

/// Resolve `jump_session_id` → SSH config (with its own ProxyJump chain) and
/// open a local forward to `dest_host:dest_port`.
async fn open_jump_tunnel(
    jump: Option<&SessionId>,
    store: Option<&SessionStore>,
    prompts: &GuiPrompts,
    dest_host: &str,
    dest_port: u16,
) -> anyhow::Result<Option<LocalTunnel>> {
    let Some(jump_id) = jump else {
        return Ok(None);
    };
    let store = store.context("SSH jump requires a session store")?;
    let jump_profile = store
        .get(jump_id.as_str())
        .cloned()
        .with_context(|| format!("jump session `{}` not found", jump_id.as_str()))?;
    if jump_profile.protocol != Protocol::Ssh {
        anyhow::bail!(
            "jump session `{}` must be SSH (got {})",
            jump_id.as_str(),
            jump_profile.protocol.as_str()
        );
    }
    let ssh_cfg = ssh_config(&jump_profile, Some(store), prompts)?;
    let tunnel = open_local_tunnel(ssh_cfg, dest_host, dest_port)
        .await
        .with_context(|| {
            format!(
                "SSH tunnel via `{}` to {dest_host}:{dest_port}",
                jump_profile.name
            )
        })?;
    Ok(Some(tunnel))
}

fn to_vnc_input(input: &DesktopInput) -> VncInput {
    match input {
        DesktopInput::Pointer { x, y, buttons } => VncInput::Pointer {
            x: *x,
            y: *y,
            buttons: *buttons,
        },
        DesktopInput::Key { down, keysym } => VncInput::Key {
            down: *down,
            keysym: *keysym,
        },
        DesktopInput::CutText(t) => VncInput::CutText(t.clone()),
        DesktopInput::CutImage(_) => VncInput::Refresh, // no RFB image clipboard
        DesktopInput::Refresh => VncInput::Refresh,
        DesktopInput::Close => VncInput::Close,
    }
}

fn to_rdp_input(input: &DesktopInput) -> RdpInput {
    match input {
        DesktopInput::Pointer { x, y, buttons } => RdpInput::Pointer {
            x: *x,
            y: *y,
            buttons: *buttons,
        },
        DesktopInput::Key { down, keysym } => RdpInput::Key {
            down: *down,
            keysym: *keysym,
        },
        DesktopInput::CutText(t) => RdpInput::CutText(t.clone()),
        DesktopInput::CutImage(png) => RdpInput::CutImage(png.clone()),
        DesktopInput::Refresh => RdpInput::Refresh,
        DesktopInput::Close => RdpInput::Close,
    }
}

fn vnc_frame(frame: gale_vnc::FrameBuffer) -> FrameBuffer {
    FrameBuffer::from_bgra(frame.width, frame.height, frame.pixels)
}

fn rdp_frame(frame: gale_rdp::FrameBuffer) -> FrameBuffer {
    FrameBuffer::from_bgra(frame.width, frame.height, frame.pixels)
}
