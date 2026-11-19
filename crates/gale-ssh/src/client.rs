use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gale_core::{AuthMethod, ProxyConfig, SessionInput, SessionProfile, SshConfig};
use russh::client::{self, ChannelOpenHandle, Config, Handle, Msg};
use russh::keys::{load_secret_key, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, ChannelOpenFailure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::host_key::{
    default_known_hosts_path, prompt_for_policy, verify_host_key, HostKeyPolicy, HostKeyPrompt,
};
use crate::proxy::dial_via_proxy;

/// Asks the user for a password or key passphrase. `None` means the user cancelled.
///
/// Called from the SSH task; implementations may block until the user answers.
pub trait SecretPrompt: Send + Sync {
    fn ask_secret(&self, prompt: &str) -> Option<String>;
}

/// Password attempts when a [`SecretPrompt`] can ask again (OpenSSH default).
const PASSWORD_ATTEMPTS: usize = 3;

/// Remote-forwarded connection delivered by the SSH server.
pub(crate) struct IncomingRemoteForward {
    pub channel: Channel<Msg>,
}

pub(crate) struct Handler {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    policy: HostKeyPolicy,
    prompt: Arc<dyn HostKeyPrompt>,
    remote_forward_tx: Option<mpsc::UnboundedSender<IncomingRemoteForward>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        match verify_host_key(
            &self.host,
            self.port,
            server_public_key,
            &self.known_hosts,
            self.policy,
            self.prompt.as_ref(),
        ) {
            Ok(accepted) => Ok(accepted),
            Err(Error::HostKeyChanged {
                host,
                line,
                fingerprint,
                known_hosts,
            }) => {
                // Surface the full warning on stderr; russh only sees a generic reject.
                eprintln!(
                    "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\n\
                     Host key for {host} does not match known_hosts (line {line}).\n\
                     Offered fingerprint: {fingerprint}\n\
                     known_hosts file: {known_hosts}\n\
                     Someone could be eavesdropping on you (man-in-the-middle attack).\n\
                     If you expected this change, remove the old key and try again."
                );
                Ok(false)
            }
            Err(Error::HostKeyRejected { host, fingerprint }) => {
                eprintln!("Host key verification failed for {host} ({fingerprint}).");
                Ok(false)
            }
            Err(e) => {
                eprintln!("Host key verification error: {e}");
                Ok(false)
            }
        }
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> std::result::Result<(), Self::Error> {
        if let Some(tx) = &self.remote_forward_tx {
            reply.accept().await;
            let _ = tx.send(IncomingRemoteForward { channel });
        } else {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SshSessionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: gale_core::SshAuth,
    pub cols: u32,
    pub rows: u32,
    pub term: String,
    pub host_key_policy: HostKeyPolicy,
    /// Override known_hosts path; `None` → `~/.ssh/known_hosts`.
    pub known_hosts_path: Option<PathBuf>,
    /// Custom prompt; `None` → policy default (`CliHostKeyPrompt` for Ask).
    pub host_key_prompt: Option<Arc<dyn HostKeyPrompt>>,
    /// Password / passphrase prompt; `None` → read from stdin (CLI).
    pub secret_prompt: Option<Arc<dyn SecretPrompt>>,
    /// ProxyJump hops, outermost first. Nested `jumps` on hop configs are ignored.
    pub jumps: Vec<SshSessionConfig>,
    /// Outbound HTTP/SOCKS5 proxy for the TCP dial to this hop.
    pub proxy: Option<ProxyConfig>,
}

impl std::fmt::Debug for SshSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSessionConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("auth", &self.auth)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("term", &self.term)
            .field("host_key_policy", &self.host_key_policy)
            .field("known_hosts_path", &self.known_hosts_path)
            .field(
                "host_key_prompt",
                &self.host_key_prompt.as_ref().map(|_| "<prompt>"),
            )
            .field(
                "secret_prompt",
                &self.secret_prompt.as_ref().map(|_| "<prompt>"),
            )
            .field("jumps", &self.jumps.len())
            .field("proxy", &self.proxy)
            .finish()
    }
}

impl SshSessionConfig {
    pub fn from_profile(profile: &SessionProfile, cols: u32, rows: u32) -> Result<Self> {
        let ssh = profile.ssh.as_ref().ok_or(Error::MissingConfig)?;
        Ok(Self::from_ssh_config(ssh, cols, rows))
    }

    pub fn from_ssh_config(ssh: &SshConfig, cols: u32, rows: u32) -> Self {
        Self {
            host: ssh.host.clone(),
            port: ssh.port,
            username: ssh.username.clone(),
            auth: ssh.auth.clone(),
            cols,
            rows,
            term: "xterm-256color".into(),
            host_key_policy: HostKeyPolicy::Ask,
            known_hosts_path: None,
            host_key_prompt: None,
            secret_prompt: None,
            jumps: Vec::new(),
            proxy: ssh.proxy.clone(),
        }
    }

    pub fn with_jumps(mut self, jumps: Vec<SshSessionConfig>) -> Self {
        self.jumps = jumps;
        self
    }

    /// Apply UI prompts to this hop and every ProxyJump hop.
    pub fn with_prompts(
        mut self,
        host_key: Arc<dyn HostKeyPrompt>,
        secret: Arc<dyn SecretPrompt>,
    ) -> Self {
        for hop in self.jumps.iter_mut() {
            hop.host_key_prompt = Some(host_key.clone());
            hop.secret_prompt = Some(secret.clone());
        }
        self.host_key_prompt = Some(host_key);
        self.secret_prompt = Some(secret);
        self
    }
}

pub struct SshSession {
    /// Target session channel — dropped first.
    channel: russh::Channel<Msg>,
    /// Target SSH handle — dropped before jump holds.
    handle: Handle<Handler>,
    /// Jump hosts (innermost first) kept alive for the tunneled stream.
    _jumps: Vec<Handle<Handler>>,
}

/// Authenticate (with optional ProxyJump) without opening a shell.
///
/// `remote_forward_tx` is attached only to the final hop Handler so remote
/// port-forward channels can be delivered to the caller.
pub(crate) async fn connect_authenticated(
    config: SshSessionConfig,
    remote_forward_tx: Option<mpsc::UnboundedSender<IncomingRemoteForward>>,
) -> Result<(Handle<Handler>, Vec<Handle<Handler>>)> {
    let client_config = Arc::new(Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        nodelay: true,
        ..Default::default()
    });

    let jumps = config.jumps.clone();
    let mut hop_cfgs: Vec<SshSessionConfig> = jumps;
    let mut target = config;
    target.jumps.clear();
    hop_cfgs.push(target);

    let last = hop_cfgs.len() - 1;
    let mut handle =
        open_and_auth_tcp(client_config.clone(), &hop_cfgs[0], remote_tx_for(0, last, &remote_forward_tx))
            .await?;
    let mut jump_holds: Vec<Handle<Handler>> = Vec::new();

    for (i, next) in hop_cfgs.iter().enumerate().skip(1) {
        let via = &hop_cfgs[i - 1];
        tracing::info!(
            via = %format!("{}:{}", via.host, via.port),
            to = %format!("{}:{}", next.host, next.port),
            "proxy jump: opening direct-tcpip"
        );
        let channel = handle
            .channel_open_direct_tcpip(
                next.host.clone(),
                u32::from(next.port),
                "127.0.0.1",
                0,
            )
            .await
            .map_err(|e| {
                Error::Ssh(format!(
                    "direct-tcpip to {}:{} failed: {e}",
                    next.host, next.port
                ))
            })?;
        let stream = channel.into_stream();
        jump_holds.push(handle);
        handle = open_and_auth_stream(
            client_config.clone(),
            next,
            stream,
            remote_tx_for(i, last, &remote_forward_tx),
        )
        .await?;
    }

    jump_holds.reverse();
    Ok((handle, jump_holds))
}

fn remote_tx_for(
    hop_index: usize,
    last_index: usize,
    tx: &Option<mpsc::UnboundedSender<IncomingRemoteForward>>,
) -> Option<mpsc::UnboundedSender<IncomingRemoteForward>> {
    if hop_index == last_index {
        tx.clone()
    } else {
        None
    }
}

impl SshSession {
    pub async fn connect(config: SshSessionConfig) -> Result<Self> {
        let cols = config.cols;
        let rows = config.rows;
        let term = config.term.clone();
        let (handle, jump_holds) = connect_authenticated(config, None).await?;

        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(e.to_string()))?;

        channel
            .request_pty(false, &term, cols, rows, 0, 0, &[])
            .await
            .map_err(|e| Error::Ssh(e.to_string()))?;

        channel
            .request_shell(false)
            .await
            .map_err(|e| Error::Ssh(e.to_string()))?;

        Ok(Self {
            channel,
            handle,
            _jumps: jump_holds,
        })
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.channel
            .data(data)
            .await
            .map_err(|e| Error::Ssh(e.to_string()))
    }

    pub async fn resize(&mut self, cols: u32, rows: u32) -> Result<()> {
        self.channel
            .window_change(cols, rows, 0, 0)
            .await
            .map_err(|e| Error::Ssh(e.to_string()))
    }

    /// Drive the shell from UI channels until the remote side closes or the
    /// UI sends [`SessionInput::Close`] / drops `input`.
    pub async fn run_channels(
        mut self,
        mut input: mpsc::UnboundedReceiver<SessionInput>,
        output: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                cmd = input.recv() => match cmd {
                    Some(SessionInput::Data(data)) => self.write_all(&data).await?,
                    Some(SessionInput::Resize { cols, rows }) => {
                        let _ = self.resize(u32::from(cols), u32::from(rows)).await;
                    }
                    Some(SessionInput::Close) | None => {
                        let _ = self.channel.eof().await;
                        break;
                    }
                },
                msg = self.channel.wait() => match msg {
                    Some(ChannelMsg::Data { ref data })
                    | Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                        if output.send(data.to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::Eof) | None => break,
                    Some(ChannelMsg::ExitStatus { .. })
                    | Some(ChannelMsg::ExitSignal { .. }) => break,
                    _ => {}
                },
            }
        }

        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
        Ok(())
    }

    /// Relay SSH channel I/O to the current process stdin/stdout until the channel closes.
    pub async fn relay_stdio(&mut self) -> Result<()> {
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut stdin_buf = [0u8; 4096];

        loop {
            tokio::select! {
                read = stdin.read(&mut stdin_buf) => {
                    let n = read?;
                    if n == 0 {
                        let _ = self.channel.eof().await;
                        break;
                    }
                    self.write_all(&stdin_buf[..n]).await?;
                }
                msg = self.channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { ref data }) => {
                            stdout.write_all(data).await?;
                            stdout.flush().await?;
                        }
                        Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                            stdout.write_all(data).await?;
                            stdout.flush().await?;
                        }
                        Some(ChannelMsg::Eof) | None => break,
                        Some(ChannelMsg::ExitStatus { .. })
                        | Some(ChannelMsg::ExitSignal { .. }) => break,
                        _ => {}
                    }
                }
            }
        }

        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
        Ok(())
    }
}

fn make_handler(
    config: &SshSessionConfig,
    remote_forward_tx: Option<mpsc::UnboundedSender<IncomingRemoteForward>>,
) -> Result<Handler> {
    let known_hosts = match config.known_hosts_path.clone() {
        Some(p) => p,
        None => default_known_hosts_path()?,
    };
    let prompt = prompt_for_policy(config.host_key_policy, config.host_key_prompt.clone());
    Ok(Handler {
        host: config.host.clone(),
        port: config.port,
        known_hosts,
        policy: config.host_key_policy,
        prompt,
        remote_forward_tx,
    })
}

fn map_connect_error(e: russh::Error) -> Error {
    match e {
        russh::Error::UnknownKey => Error::Ssh(
            "host key verification failed (rejected, or it no longer matches ~/.ssh/known_hosts)"
                .into(),
        ),
        other => Error::Ssh(other.to_string()),
    }
}

async fn open_and_auth_tcp(
    client_config: Arc<Config>,
    config: &SshSessionConfig,
    remote_forward_tx: Option<mpsc::UnboundedSender<IncomingRemoteForward>>,
) -> Result<Handle<Handler>> {
    let handler = make_handler(config, remote_forward_tx)?;
    let stream = match &config.proxy {
        Some(proxy) => {
            tracing::info!(
                proxy = %format!("{}:{}", proxy.host, proxy.port),
                target = %format!("{}:{}", config.host, config.port),
                "dialing SSH via outbound proxy"
            );
            dial_via_proxy(proxy, &config.host, config.port).await?
        }
        None => {
            let stream = TcpStream::connect((config.host.as_str(), config.port))
                .await
                .map_err(|e| {
                    Error::Ssh(format!(
                        "failed to connect to {}:{}: {e}",
                        config.host, config.port
                    ))
                })?;
            stream.set_nodelay(true).ok();
            stream
        }
    };
    let mut handle = client::connect_stream(client_config, stream, handler)
        .await
        .map_err(map_connect_error)?;
    if !authenticate(&mut handle, config).await? {
        return Err(Error::AuthFailed);
    }
    Ok(handle)
}

async fn open_and_auth_stream<R>(
    client_config: Arc<Config>,
    config: &SshSessionConfig,
    stream: R,
    remote_forward_tx: Option<mpsc::UnboundedSender<IncomingRemoteForward>>,
) -> Result<Handle<Handler>>
where
    R: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let handler = make_handler(config, remote_forward_tx)?;
    let mut handle = client::connect_stream(client_config, stream, handler)
        .await
        .map_err(map_connect_error)?;
    if !authenticate(&mut handle, config).await? {
        return Err(Error::AuthFailed);
    }
    Ok(handle)
}

/// Try the configured method; like OpenSSH, fall back to a password prompt when
/// key / agent auth fails and the server still accepts passwords.
async fn authenticate(handle: &mut Handle<Handler>, config: &SshSessionConfig) -> Result<bool> {
    let key_result = match config.auth.method {
        AuthMethod::Password | AuthMethod::KeyboardInteractive => {
            return authenticate_password(handle, config).await;
        }
        AuthMethod::PublicKey => authenticate_publickey(handle, config).await,
        AuthMethod::Agent => {
            crate::agent::authenticate_with_agent(handle, &config.username).await
        }
    };
    match key_result {
        Ok(true) => return Ok(true),
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Ok(false) => tracing::debug!("{:?} auth rejected", config.auth.method),
        Err(e) => tracing::debug!("{:?} auth failed: {e}", config.auth.method),
    }
    if !server_accepts_password(handle, &config.username).await {
        return Ok(false);
    }
    authenticate_password(handle, config).await
}

/// Ask the server which methods remain via a `none` request.
async fn server_accepts_password(handle: &mut Handle<Handler>, user: &str) -> bool {
    match handle.authenticate_none(user).await {
        Ok(russh::client::AuthResult::Failure {
            remaining_methods, ..
        }) => remaining_methods.contains(&russh::MethodKind::Password),
        _ => false,
    }
}

async fn authenticate_password(
    handle: &mut Handle<Handler>,
    config: &SshSessionConfig,
) -> Result<bool> {
    let prompt = format!("{}@{}'s password: ", config.username, config.host);
    if let Some(password) = &config.auth.password {
        return try_password(handle, &config.username, password.clone()).await;
    }
    let attempts = if config.secret_prompt.is_some() {
        PASSWORD_ATTEMPTS
    } else {
        1
    };
    for _ in 0..attempts {
        let password = ask_secret(config, &prompt)?;
        if try_password(handle, &config.username, password).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn authenticate_publickey(
    handle: &mut Handle<Handler>,
    config: &SshSessionConfig,
) -> Result<bool> {
    let key_path = resolve_key_path(config.auth.private_key_path.as_deref())?;
    let key = match (
        load_secret_key(&key_path, config.auth.passphrase.as_deref()),
        &config.secret_prompt,
    ) {
        (Ok(key), _) => key,
        (Err(russh::keys::Error::KeyIsEncrypted), Some(_)) => {
            let passphrase = ask_secret(
                config,
                &format!("Enter passphrase for key '{}': ", key_path.display()),
            )?;
            load_key(&key_path, Some(&passphrase))?
        }
        (Err(e), _) => return Err(Error::Ssh(e.to_string())),
    };
    let hash_alg = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| Error::Ssh(e.to_string()))?
        .flatten();
    let keyed = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    let result = handle
        .authenticate_publickey(&config.username, keyed)
        .await
        .map_err(|e| Error::Ssh(e.to_string()))?;
    Ok(result.success())
}

fn resolve_key_path(explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(expand_tilde(path));
    }
    let home = dirs_next_home()?;
    for name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
        let candidate = home.join(".ssh").join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::Ssh(
        "no private key found; set --identity or place ~/.ssh/id_ed25519".into(),
    ))
}

fn load_key(path: &PathBuf, passphrase: Option<&str>) -> Result<PrivateKey> {
    load_secret_key(path, passphrase).map_err(|e| Error::Ssh(e.to_string()))
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = dirs_next_home() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn dirs_next_home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| Error::Ssh("cannot resolve home directory".into()))
}

async fn try_password(handle: &mut Handle<Handler>, user: &str, password: String) -> Result<bool> {
    let result = handle
        .authenticate_password(user, password)
        .await
        .map_err(|e| Error::Ssh(e.to_string()))?;
    Ok(result.success())
}

fn ask_secret(config: &SshSessionConfig, prompt: &str) -> Result<String> {
    match &config.secret_prompt {
        Some(p) => p.ask_secret(prompt).ok_or(Error::Cancelled),
        None => prompt_secret(prompt),
    }
}

fn prompt_secret(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim_end_matches(['\r', '\n']).to_string())
}
