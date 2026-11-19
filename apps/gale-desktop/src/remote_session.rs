//! Remote sessions (SSH / Telnet / TCP / Serial) driven on a background tokio
//! runtime and pumped into a [`gale_term_view::TerminalPane`].
//!
//! Interactive prompts (unknown host key, password, key passphrase) are sent
//! to the UI as [`PromptRequest`]s; the network task blocks until answered.

use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, OnceLock};

use anyhow::{bail, Context as _};
use gale_core::{Protocol, SessionInput, SessionProfile, SessionStore};
use gale_ftp::{CertDecision, CertInfo, FtpPrompt};
use gale_net::{TcpSession, TcpSessionConfig, TelnetSession, TelnetSessionConfig};
use gale_serial::{SerialSession, SerialSessionConfig};
use gale_ssh::{
    HostKeyDecision, HostKeyInfo, HostKeyPrompt, SecretPrompt, SshSession, SshSessionConfig,
};
use gale_term_view::{TerminalPane, TerminalPaneEvent, TerminalResized};
use gpui_kit::*;
use tokio::sync::mpsc;

pub const COLS: u16 = 100;
pub const ROWS: u16 = 30;

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("gale-net")
            .enable_all()
            .build()
            .expect("failed to start tokio runtime")
    })
}

/// A question the network task needs the user to answer.
pub enum PromptRequest {
    HostKey {
        info: HostKeyInfo,
        reply: std_mpsc::Sender<HostKeyDecision>,
    },
    Secret {
        prompt: String,
        reply: std_mpsc::Sender<Option<String>>,
    },
    /// FTPS server certificate not trusted by the system.
    Certificate {
        info: CertInfo,
        reply: std_mpsc::Sender<CertDecision>,
    },
}

/// Bridges gale-ssh prompt callbacks to the UI thread.
#[derive(Clone)]
pub struct GuiPrompts {
    tx: async_channel::Sender<PromptRequest>,
}

impl GuiPrompts {
    pub fn new(tx: async_channel::Sender<PromptRequest>) -> Self {
        Self { tx }
    }
}

impl HostKeyPrompt for GuiPrompts {
    fn confirm_unknown(&self, info: &HostKeyInfo) -> HostKeyDecision {
        let (reply, answer) = std_mpsc::channel();
        let req = PromptRequest::HostKey {
            info: info.clone(),
            reply,
        };
        if self.tx.send_blocking(req).is_err() {
            return HostKeyDecision::Reject;
        }
        answer.recv().unwrap_or(HostKeyDecision::Reject)
    }
}

impl SecretPrompt for GuiPrompts {
    fn ask_secret(&self, prompt: &str) -> Option<String> {
        let (reply, answer) = std_mpsc::channel();
        let req = PromptRequest::Secret {
            prompt: prompt.trim().trim_end_matches(':').to_string(),
            reply,
        };
        self.tx.send_blocking(req).ok()?;
        answer.recv().ok().flatten()
    }
}

/// SSH parameters for `profile` (SSH or SFTP), with GUI prompts on every hop.
/// Saved profiles resolve their ProxyJump chain through `store`.
pub fn ssh_config(
    profile: &SessionProfile,
    store: Option<&SessionStore>,
    prompts: &GuiPrompts,
) -> anyhow::Result<SshSessionConfig> {
    let chain = match store {
        Some(store) if store.get(profile.id.as_str()).is_some() => store
            .resolve_jump_chain(profile.id.as_str())
            .map_err(|e| anyhow::anyhow!(e))?,
        _ => vec![profile.clone()],
    };
    let mut hops = chain
        .iter()
        .map(|p| SshSessionConfig::from_profile(p, COLS.into(), ROWS.into()))
        .collect::<Result<Vec<_>, _>>()?;
    let target = hops.pop().context("empty jump chain")?;
    let prompts = Arc::new(prompts.clone());
    Ok(target.with_jumps(hops).with_prompts(prompts.clone(), prompts))
}

impl FtpPrompt for GuiPrompts {
    fn ask_password(&self, prompt: &str) -> Option<String> {
        self.ask_secret(prompt)
    }

    fn confirm_certificate(&self, info: &CertInfo) -> CertDecision {
        let (reply, answer) = std_mpsc::channel();
        let req = PromptRequest::Certificate {
            info: info.clone(),
            reply,
        };
        if self.tx.send_blocking(req).is_err() {
            return CertDecision::Reject;
        }
        answer.recv().unwrap_or(CertDecision::Reject)
    }
}

/// Protocol-specific connection parameters, resolved on the UI thread.
pub enum RemoteTarget {
    Ssh(Box<SshSessionConfig>),
    Telnet(TelnetSessionConfig),
    Tcp(TcpSessionConfig),
    Serial(SerialSessionConfig),
}

impl RemoteTarget {
    /// Build a target for `profile`. Saved SSH profiles resolve their
    /// ProxyJump chain through `store`.
    pub fn from_profile(
        profile: &SessionProfile,
        store: Option<&SessionStore>,
        prompts: &GuiPrompts,
    ) -> anyhow::Result<Self> {
        Ok(match profile.protocol {
            Protocol::Ssh => Self::Ssh(Box::new(ssh_config(profile, store, prompts)?)),
            Protocol::Telnet => {
                Self::Telnet(TelnetSessionConfig::from_profile(profile, COLS, ROWS)?)
            }
            Protocol::Tcp => Self::Tcp(TcpSessionConfig::from_profile(profile)?),
            Protocol::Serial => Self::Serial(SerialSessionConfig::from_profile(profile)?),
            other => bail!("{} is not an interactive remote protocol", other.as_str()),
        })
    }

    async fn run(
        self,
        input: mpsc::UnboundedReceiver<SessionInput>,
        output: mpsc::UnboundedSender<Vec<u8>>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Ssh(cfg) => {
                let session = SshSession::connect(*cfg).await?;
                session.run_channels(input, output).await?;
            }
            Self::Telnet(cfg) => {
                let session = TelnetSession::connect(cfg).await?;
                session.run_channels(input, output).await?;
            }
            Self::Tcp(cfg) => {
                let session = TcpSession::connect(cfg).await?;
                session.run_channels(input, output).await?;
            }
            Self::Serial(cfg) => {
                let session = SerialSession::open(cfg).await?;
                session.run_channels(input, output).await?;
            }
        }
        Ok(())
    }
}

pub struct RemoteTerminalTab {
    pub pane: Entity<TerminalPane>,
    input: mpsc::UnboundedSender<SessionInput>,
}

impl RemoteTerminalTab {
    /// Start connecting in the background. `on_closed` runs on the UI thread
    /// once the session ends (cleanly or with an error).
    pub fn connect(
        target: RemoteTarget,
        label: String,
        on_closed: impl FnOnce(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let pane = cx.new(|_cx| TerminalPane::new(COLS.into(), ROWS.into()));
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        runtime().spawn(async move {
            let _ = output_tx.send(status_line(&format!("Connecting to {label}…"), DIM));
            let result = target.run(input_rx, output_tx.clone()).await;
            let msg = match result {
                Ok(()) => status_line("Session closed.", DIM),
                Err(err) => status_line(&format!("{err:#}"), RED),
            };
            let _ = output_tx.send(msg);
        });

        let pane_input_tx = input_tx.clone();
        cx.subscribe(&pane, move |_, ev: &TerminalPaneEvent, _| {
            if let TerminalPaneEvent::Output(data) = ev {
                let _ = pane_input_tx.send(SessionInput::Data(data.clone()));
            }
        })
        .detach();

        let resize_tx = input_tx.clone();
        cx.subscribe(&pane, move |_, ev: &TerminalResized, _| {
            let _ = resize_tx.send(SessionInput::Resize {
                cols: ev.cols.min(u16::MAX as usize) as u16,
                rows: ev.rows.min(u16::MAX as usize) as u16,
            });
        })
        .detach();

        let pane_for_task = pane.downgrade();
        let replies = input_tx.clone();
        window
            .spawn(cx, async move |cx| {
                while let Some(mut chunk) = output_rx.recv().await {
                    while let Ok(more) = output_rx.try_recv() {
                        chunk.extend_from_slice(&more);
                    }
                    let fed = pane_for_task.update(cx, |pane, cx| {
                        for ev in pane.feed_bytes(&chunk, cx) {
                            if let TerminalPaneEvent::Output(data) = ev {
                                let _ = replies.send(SessionInput::Data(data));
                            }
                        }
                    });
                    if fed.is_err() {
                        return;
                    }
                }
                let _ = cx.update(|window, cx| on_closed(window, cx));
            })
            .detach();

        Self {
            pane,
            input: input_tx,
        }
    }

    pub fn write_input(&self, data: &[u8]) {
        let _ = self.input.send(SessionInput::Data(data.to_vec()));
    }
}

impl Drop for RemoteTerminalTab {
    fn drop(&mut self) {
        let _ = self.input.send(SessionInput::Close);
    }
}

const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";

fn status_line(text: &str, sgr: &str) -> Vec<u8> {
    format!("\r\n{sgr}{}\x1b[0m\r\n", text.replace('\n', "\r\n")).into_bytes()
}
