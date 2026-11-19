//! VNC (and later RDP) sessions: network task ↔ [`DesktopPane`].

use std::sync::Arc;

use anyhow::Context as _;
use gale_core::vault::CredentialKey;
use gale_core::{SessionProfile, SessionStore};
use gale_desktop_view::{DesktopPane, DesktopPaneEvent};
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

pub struct RemoteDesktopTab {
    pub pane: Entity<DesktopPane>,
    input: mpsc::UnboundedSender<VncInput>,
}

impl RemoteDesktopTab {
    pub fn connect(
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
            // Tunnel support lands with the rest of Phase 3.5; refuse clearly for now.
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
            let _ = pane_input.send(input.clone());
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
                        VncOutput::Frame(frame) => pane.set_frame(frame, cx),
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
            input: input_tx,
        })
    }
}

impl Drop for RemoteDesktopTab {
    fn drop(&mut self) {
        let _ = self.input.send(VncInput::Close);
    }
}
