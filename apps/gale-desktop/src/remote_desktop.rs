//! VNC / RDP sessions: network task ↔ [`DesktopPane`].

use std::sync::Arc;

use anyhow::Context as _;
use gale_core::vault::CredentialKey;
use gale_core::{Protocol, SessionId, SessionProfile, SessionStore};
use gale_desktop_view::{DesktopInput, DesktopPane, DesktopPaneEvent, FrameBuffer};
use gale_rdp::{RdpClientConfig, RdpInput, RdpOutput, RdpPrompt, RdpSession};
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
}

enum DesktopBackend {
    Vnc(mpsc::UnboundedSender<VncInput>),
    Rdp(mpsc::UnboundedSender<RdpInput>),
}

pub struct RemoteDesktopTab {
    pub pane: Entity<DesktopPane>,
    backend: DesktopBackend,
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
            Protocol::Vnc => Self::connect_vnc(profile, prompts, on_closed, window, cx),
            Protocol::Rdp => Self::connect_rdp(profile, prompts, on_closed, window, cx),
            other => anyhow::bail!("not a remote-desktop protocol: {}", other.as_str()),
        }
    }

    fn connect_vnc(
        profile: SessionProfile,
        prompts: GuiPrompts,
        on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<Self> {
        let store = SessionStore::open_default().ok();
        let mut config = VncClientConfig::from_profile(&profile)?;
        let jump = profile
            .vnc
            .as_ref()
            .and_then(|v| v.jump_session_id.clone());
        let dest_host = config.host.clone();
        let dest_port = config.port;
        config = config.with_prompt(Arc::new(prompts.clone()));

        let pane = cx.new(|cx| DesktopPane::new(format!("vnc-pane-{}", profile.id.as_str()), cx));
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
        cx.subscribe(&pane, move |_, ev: &DesktopPaneEvent, _| {
            let DesktopPaneEvent::Input(input) = ev;
            match input {
                DesktopInput::CutText(text) => {
                    // Update remote cut buffer, then Ctrl+V so the focused app pastes.
                    let _ = pane_input.send(VncInput::CutText(text.clone()));
                    for (down, keysym) in [
                        (true, 0xffe3_u32),  // Control_L
                        (true, 0x0076),      // v
                        (false, 0x0076),
                        (false, 0xffe3),
                    ] {
                        let _ = pane_input.send(VncInput::Key { down, keysym });
                    }
                }
                other => {
                    let _ = pane_input.send(to_vnc_input(other));
                }
            }
        })
        .detach();

        let pane_task = pane.downgrade();
        window
            .spawn(cx, async move |cx| {
                while let Some(msg) = output_rx.recv().await {
                    let alive = pane_task.update(cx, |pane, cx| match msg {
                        VncOutput::Ready {
                            width,
                            height,
                            name,
                        } => pane.set_ready(width, height, name, cx),
                        VncOutput::Frame(frame) => {
                            pane.set_frame(vnc_frame(frame), cx);
                        }
                        VncOutput::Bell => {}
                        VncOutput::CutText(text) if text.starts_with("error:") => {
                            pane.set_status(text.trim_start_matches("error:").to_string(), cx);
                        }
                        VncOutput::CutText(text) => write_clipboard(cx, text),
                    });
                    if alive.is_err() {
                        return;
                    }
                }
                let _ = cx.update(|window, cx| on_closed(window, cx));
            })
            .detach();

        Ok(Self {
            pane,
            backend: DesktopBackend::Vnc(input_tx),
        })
    }

    fn connect_rdp(
        profile: SessionProfile,
        prompts: GuiPrompts,
        on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<Self> {
        let store = SessionStore::open_default().ok();
        let mut config = RdpClientConfig::from_profile(&profile)?;
        let jump = profile
            .rdp
            .as_ref()
            .and_then(|v| v.jump_session_id.clone());
        let dest_host = config.host.clone();
        let dest_port = config.port;
        config = config.with_prompt(Arc::new(prompts.clone()));

        let pane = cx.new(|cx| DesktopPane::new(format!("rdp-pane-{}", profile.id.as_str()), cx));
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
        cx.subscribe(&pane, move |_, ev: &DesktopPaneEvent, _| {
            let DesktopPaneEvent::Input(input) = ev;
            let _ = pane_input.send(to_rdp_input(input));
        })
        .detach();

        let pane_task = pane.downgrade();
        window
            .spawn(cx, async move |cx| {
                while let Some(msg) = output_rx.recv().await {
                    let alive = pane_task.update(cx, |pane, cx| match msg {
                        RdpOutput::Ready {
                            width,
                            height,
                            name,
                        } => pane.set_ready(width, height, name, cx),
                        RdpOutput::Frame(frame) => pane.set_frame(rdp_frame(frame), cx),
                        RdpOutput::CutText(text) if text.starts_with("error:") => {
                            pane.set_status(text.trim_start_matches("error:").to_string(), cx);
                        }
                        RdpOutput::CutText(text) => write_clipboard(cx, text),
                    });
                    if alive.is_err() {
                        return;
                    }
                }
                let _ = cx.update(|window, cx| on_closed(window, cx));
            })
            .detach();

        Ok(Self {
            pane,
            backend: DesktopBackend::Rdp(input_tx),
        })
    }
}

impl Drop for RemoteDesktopTab {
    fn drop(&mut self) {
        match &self.backend {
            DesktopBackend::Vnc(tx) => {
                let _ = tx.send(VncInput::Close);
            }
            DesktopBackend::Rdp(tx) => {
                let _ = tx.send(RdpInput::Close);
            }
        }
    }
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
