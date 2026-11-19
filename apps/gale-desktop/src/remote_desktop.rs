//! VNC / RDP sessions: network task ↔ [`DesktopPane`].

use std::sync::Arc;

use anyhow::Context as _;
use gale_core::vault::CredentialKey;
use gale_core::{Protocol, SessionProfile, SessionStore};
use gale_desktop_view::{DesktopInput, DesktopPane, DesktopPaneEvent, FrameBuffer};
use gale_rdp::{RdpClientConfig, RdpInput, RdpOutput, RdpPrompt, RdpSession};
use gale_vnc::{VncClientConfig, VncInput, VncOutput, VncPrompt, VncSession};
use gpui_kit::*;
use tokio::sync::mpsc;

use crate::remote_session::{runtime, GuiPrompts};

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
        if let Some(jump) = profile
            .vnc
            .as_ref()
            .and_then(|v| v.jump_session_id.as_ref())
        {
            let _ = store;
            anyhow::bail!(
                "VNC via SSH jump (`{}`) is not implemented yet",
                jump.as_str()
            );
        }
        config = config.with_prompt(Arc::new(prompts));

        let pane = cx.new(|cx| DesktopPane::new(format!("vnc-pane-{}", profile.id.as_str()), cx));
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel::<VncOutput>();
        let label = profile.name.clone();

        runtime().spawn(async move {
            let result = async {
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
            let _ = pane_input.send(to_vnc_input(input));
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
                        VncOutput::CutText(_) => {}
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
        if let Some(jump) = profile
            .rdp
            .as_ref()
            .and_then(|v| v.jump_session_id.as_ref())
        {
            let _ = store;
            anyhow::bail!(
                "RDP via SSH jump (`{}`) is not implemented yet",
                jump.as_str()
            );
        }
        config = config.with_prompt(Arc::new(prompts));

        let pane = cx.new(|cx| DesktopPane::new(format!("rdp-pane-{}", profile.id.as_str()), cx));
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel::<RdpOutput>();
        let label = profile.name.clone();

        // IronRDP blocking I/O — keep it off the UI / async workers.
        runtime().spawn_blocking(move || {
            let result = (|| {
                let session =
                    RdpSession::connect(config).with_context(|| format!("connecting to {label}"))?;
                session.run(input_rx, output_tx.clone())?;
                Ok::<(), anyhow::Error>(())
            })();
            if let Err(err) = result {
                let _ = output_tx.send(RdpOutput::CutText(format!("error:{err:#}")));
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
                        RdpOutput::CutText(_) => {}
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
