//! Remote sessions (SSH / Telnet / TCP / Serial) driven on a background tokio
//! runtime and pumped into a [`gale_term_view::TerminalPane`].
//!
//! Interactive prompts (unknown host key, password, key passphrase) are sent
//! to the UI as [`PromptRequest`]s; the network task blocks until answered.

use std::collections::HashSet;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context as _};
use gale_core::vault::{CredentialKey, Vault};
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
        /// `Some(initial)` shows a "remember in keychain" checkbox.
        remember: Option<bool>,
        /// The saved secret for this credential was just rejected.
        stale: bool,
        reply: std_mpsc::Sender<Option<SecretAnswer>>,
    },
    /// FTPS server certificate not trusted by the system.
    Certificate {
        info: CertInfo,
        reply: std_mpsc::Sender<CertDecision>,
    },
}

pub struct SecretAnswer {
    pub value: String,
    pub remember: bool,
}

#[derive(Default)]
struct CredentialState {
    /// Credentials the user asked to remember, saved once they work.
    remember: HashSet<CredentialKey>,
    /// Credentials handed out from the vault, awaiting their result.
    from_vault: HashSet<CredentialKey>,
    /// Saved credentials the server rejected.
    stale: HashSet<CredentialKey>,
}

/// Bridges gale-ssh / gale-ftp prompt callbacks to the UI thread, and keeps
/// remembered secrets in the system keychain.
#[derive(Clone)]
pub struct GuiPrompts {
    tx: async_channel::Sender<PromptRequest>,
    vault: Option<Vault>,
    state: Arc<Mutex<CredentialState>>,
}

impl GuiPrompts {
    pub fn new(tx: async_channel::Sender<PromptRequest>, vault: Option<Vault>) -> Self {
        Self {
            tx,
            vault,
            state: Arc::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CredentialState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn ask(&self, prompt: &str, remember: Option<bool>, stale: bool) -> Option<SecretAnswer> {
        let (reply, answer) = std_mpsc::channel();
        let req = PromptRequest::Secret {
            prompt: prompt.trim().trim_end_matches(':').to_string(),
            remember,
            stale,
            reply,
        };
        self.tx.send_blocking(req).ok()?;
        answer.recv().ok().flatten()
    }

    pub(crate) fn ask_and_track(&self, key: &CredentialKey, prompt: &str) -> Option<String> {
        if self.vault.is_none() {
            return self.ask(prompt, None, false).map(|a| a.value);
        }
        // Someone who saved this before probably wants the new one saved too.
        let stale = self.state().stale.contains(key);
        let answer = self.ask(prompt, Some(stale), stale)?;
        let mut state = self.state();
        if answer.remember {
            state.remember.insert(key.clone());
        } else {
            state.remember.remove(key);
        }
        Some(answer.value)
    }

    pub(crate) fn saved(&self, key: &CredentialKey) -> Option<String> {
        let vault = self.vault.as_ref()?;
        match vault.get(key) {
            Ok(Some(secret)) => {
                self.state().from_vault.insert(key.clone());
                Some(secret)
            }
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(%err, key = key.as_str(), "reading saved credential failed");
                None
            }
        }
    }

    pub(crate) fn record(&self, key: &CredentialKey, secret: &str, accepted: bool) {
        let Some(vault) = &self.vault else {
            return;
        };
        let (save, was_saved) = {
            let mut state = self.state();
            let was_saved = state.from_vault.remove(key);
            if accepted {
                state.stale.remove(key);
            } else if was_saved {
                state.stale.insert(key.clone());
            }
            (accepted && state.remember.remove(key), was_saved)
        };
        if save && !was_saved {
            if let Err(err) = vault.set(key, secret) {
                tracing::warn!(%err, key = key.as_str(), "saving credential failed");
            }
        }
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

    fn ask_credential(&self, key: &CredentialKey, prompt: &str) -> Option<String> {
        self.ask_and_track(key, prompt)
    }

    fn saved_credential(&self, key: &CredentialKey) -> Option<String> {
        self.saved(key)
    }

    fn credential_result(&self, key: &CredentialKey, secret: &str, accepted: bool) {
        self.record(key, secret, accepted);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gale_core::vault::{CredentialKey, MemoryStore, Vault};

    use super::GuiPrompts;

    fn prompts() -> (GuiPrompts, Vault, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("gale-prompts-{}", uuid_like()));
        let vault = Vault::new(Arc::new(MemoryStore::default()), dir.join("index.json"));
        let (tx, _rx) = async_channel::unbounded();
        (GuiPrompts::new(tx, Some(vault.clone())), vault, dir)
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn remembered_secret_is_saved_only_once_accepted() {
        let (prompts, vault, dir) = prompts();
        let key = CredentialKey::password("ssh", "u", "h", 22);

        prompts.state().remember.insert(key.clone());
        prompts.record(&key, "wrong", false);
        assert_eq!(vault.get(&key).unwrap(), None);

        prompts.record(&key, "right", true);
        assert_eq!(vault.get(&key).unwrap().as_deref(), Some("right"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejected_saved_secret_is_marked_stale_not_deleted() {
        let (prompts, vault, dir) = prompts();
        let key = CredentialKey::password("ftp", "u", "h", 21);
        vault.set(&key, "old").unwrap();

        assert_eq!(prompts.saved(&key).as_deref(), Some("old"));
        prompts.record(&key, "old", false);
        assert!(prompts.state().stale.contains(&key));
        assert_eq!(vault.get(&key).unwrap().as_deref(), Some("old"));

        // The user re-enters it with "remember" ticked.
        prompts.state().remember.insert(key.clone());
        prompts.record(&key, "new", true);
        assert!(!prompts.state().stale.contains(&key));
        assert_eq!(vault.get(&key).unwrap().as_deref(), Some("new"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
