//! FTP / FTPS client: one control connection per client, passive-mode data
//! connections, and [`FtpClient::fork`] for transfers that should not block
//! browsing on the main connection.

use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use gale_core::{FtpSecurity, SessionProfile};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};
use crate::list::{parse_list, parse_mlsd, FtpEntry};
use crate::tls::{client_config, default_known_certs_path, FtpPrompt, Stream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const PASSWORD_ATTEMPTS: usize = 3;
const COPY_CHUNK: usize = 64 * 1024;

/// Called with the number of bytes moved since the last call; return `false`
/// to abort the transfer with [`Error::Cancelled`].
pub type TransferProgress<'a> = &'a mut (dyn FnMut(u64) -> bool + Send);

#[derive(Clone)]
pub struct FtpClientConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub security: FtpSecurity,
    pub prompt: Option<Arc<dyn FtpPrompt>>,
    /// Where certificates accepted with "save" are remembered.
    pub known_certs_path: Option<PathBuf>,
}

impl FtpClientConfig {
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: None,
            security: FtpSecurity::Auto,
            prompt: None,
            known_certs_path: default_known_certs_path(),
        }
    }

    pub fn from_profile(profile: &SessionProfile) -> Result<Self> {
        let ftp = profile.ftp.as_ref().ok_or(Error::MissingConfig)?;
        let mut config = Self::new(ftp.host.clone(), ftp.port, ftp.username.clone());
        config.password = ftp.password.clone();
        config.security = ftp.security;
        Ok(config)
    }

    pub fn with_prompt(mut self, prompt: Arc<dyn FtpPrompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }
}

#[derive(Debug)]
struct Reply {
    code: u16,
    text: String,
}

impl Reply {
    fn into_error(self) -> Error {
        Error::Reply {
            code: self.code,
            message: self.text,
        }
    }
}

#[derive(Clone)]
struct TlsCtx {
    connector: TlsConnector,
    name: ServerName<'static>,
}

impl TlsCtx {
    async fn handshake(&self, tcp: TcpStream) -> Result<TlsStream<TcpStream>> {
        timeout(CONNECT_TIMEOUT, self.connector.connect(self.name.clone(), tcp))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("UnsupportedCertVersion") {
                    Error::Tls(format!(
                        "{msg} (the server uses an X.509 v1 certificate; regenerate it as v3)"
                    ))
                } else {
                    Error::Tls(msg)
                }
            })
    }
}

/// State shared by a client and its forks.
struct Shared {
    config: FtpClientConfig,
    tls: Option<Arc<ClientConfig>>,
    /// Password that last logged in, so forks don't ask again.
    password: StdMutex<Option<String>>,
    secure: AtomicBool,
}

struct Control {
    stream: BufReader<Stream>,
    peer: IpAddr,
    tls: Option<TlsCtx>,
    mlsd: bool,
    epsv: bool,
}

async fn dial(host: &str, port: u16) -> Result<TcpStream> {
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Timeout)??;
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

impl Control {
    async fn connect(shared: &Shared) -> Result<Self> {
        let cfg = &shared.config;
        let tcp = dial(&cfg.host, cfg.port).await?;
        let peer = tcp.peer_addr()?.ip();
        let tls = match &shared.tls {
            Some(config) => Some(TlsCtx {
                connector: TlsConnector::from(config.clone()),
                name: ServerName::try_from(cfg.host.clone())
                    .map_err(|e| Error::Tls(e.to_string()))?,
            }),
            None => None,
        };

        let implicit = cfg.security == FtpSecurity::Implicit;
        let stream = match (&tls, implicit) {
            (Some(ctx), true) => Stream::Tls(Box::new(ctx.handshake(tcp).await?)),
            _ => Stream::Plain(tcp),
        };
        let mut ctl = Self {
            stream: BufReader::new(stream),
            peer,
            tls: if implicit { tls.clone() } else { None },
            mlsd: false,
            epsv: true,
        };

        let greeting = ctl.read_reply().await?;
        if greeting.code != 220 {
            return Err(greeting.into_error());
        }

        if let (Some(ctx), FtpSecurity::Auto | FtpSecurity::Explicit) = (tls, cfg.security) {
            let reply = ctl.command("AUTH TLS").await?;
            if reply.code == 234 {
                ctl = ctl.upgrade(ctx).await?;
            } else if cfg.security == FtpSecurity::Explicit {
                return Err(Error::TlsUnavailable);
            }
        }
        if ctl.tls.is_some() {
            ctl.command("PBSZ 0").await?;
            ctl.expect("PROT P", &[200]).await?;
        }
        shared.secure.store(ctl.tls.is_some(), Ordering::Relaxed);

        ctl.login(shared).await?;

        if let Ok(reply) = ctl.command("FEAT").await {
            if reply.code == 211 {
                let features: Vec<String> = reply
                    .text
                    .lines()
                    .map(|l| l.trim().to_ascii_uppercase())
                    .collect();
                ctl.mlsd = features.iter().any(|f| f.starts_with("MLST"));
                if features.iter().any(|f| f == "UTF8") {
                    let _ = ctl.command("OPTS UTF8 ON").await;
                }
            }
        }
        ctl.expect("TYPE I", &[200]).await?;
        Ok(ctl)
    }

    async fn upgrade(self, ctx: TlsCtx) -> Result<Self> {
        let Stream::Plain(tcp) = self.stream.into_inner() else {
            return Err(Error::Protocol("control connection is already encrypted".into()));
        };
        let tls = ctx.handshake(tcp).await?;
        Ok(Self {
            stream: BufReader::new(Stream::Tls(Box::new(tls))),
            peer: self.peer,
            tls: Some(ctx),
            mlsd: self.mlsd,
            epsv: self.epsv,
        })
    }

    async fn login(&mut self, shared: &Shared) -> Result<()> {
        let cfg = &shared.config;
        let user = if cfg.username.is_empty() {
            "anonymous"
        } else {
            cfg.username.as_str()
        };
        let anonymous = user.eq_ignore_ascii_case("anonymous") || user.eq_ignore_ascii_case("ftp");
        for attempt in 0..PASSWORD_ATTEMPTS {
            let reply = self.command(&format!("USER {user}")).await?;
            match reply.code {
                230 => return Ok(()),
                331 | 332 => {}
                _ => return Err(Error::Login(reply.text)),
            }
            let remembered = if attempt == 0 {
                shared
                    .password
                    .lock()
                    .ok()
                    .and_then(|p| p.clone())
                    .or_else(|| cfg.password.clone())
            } else {
                None
            };
            let password = match remembered {
                Some(p) => p,
                None if anonymous => "anonymous@".to_string(),
                None => cfg
                    .prompt
                    .as_ref()
                    .and_then(|p| p.ask_password(&format!("Password for {user}@{}", cfg.host)))
                    .ok_or(Error::Cancelled)?,
            };
            let reply = self.command(&format!("PASS {password}")).await?;
            match reply.code {
                230 | 202 => {
                    if let Ok(mut slot) = shared.password.lock() {
                        *slot = Some(password);
                    }
                    return Ok(());
                }
                530 if !anonymous && cfg.prompt.is_some() && attempt + 1 < PASSWORD_ATTEMPTS => {
                    tracing::debug!("ftp login rejected, asking again");
                }
                _ => return Err(Error::Login(reply.text)),
            }
        }
        Err(Error::Login("too many failed attempts".into()))
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = timeout(REPLY_TIMEOUT, self.stream.read_until(b'\n', &mut buf))
            .await
            .map_err(|_| Error::Timeout)??;
        if n == 0 {
            return Err(Error::Closed);
        }
        Ok(String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string())
    }

    async fn read_reply(&mut self) -> Result<Reply> {
        let first = self.read_line().await?;
        let code: u16 = first
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| Error::Protocol(format!("malformed reply: {first}")))?;
        let mut text = first.get(4..).unwrap_or_default().to_string();
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let line = self.read_line().await?;
                text.push('\n');
                if line.starts_with(&end) || line == code.to_string() {
                    text.push_str(line.get(4..).unwrap_or_default());
                    break;
                }
                text.push_str(&line);
            }
        }
        Ok(Reply { code, text })
    }

    async fn command(&mut self, line: &str) -> Result<Reply> {
        if line.contains(['\r', '\n']) {
            return Err(Error::Protocol("line breaks are not allowed in FTP paths".into()));
        }
        if line.starts_with("PASS ") {
            tracing::trace!("ftp > PASS ****");
        } else {
            tracing::trace!("ftp > {line}");
        }
        let stream = self.stream.get_mut();
        stream.write_all(format!("{line}\r\n").as_bytes()).await?;
        stream.flush().await?;
        let reply = self.read_reply().await?;
        tracing::trace!("ftp < {} {}", reply.code, reply.text);
        Ok(reply)
    }

    async fn expect(&mut self, line: &str, ok: &[u16]) -> Result<Reply> {
        let reply = self.command(line).await?;
        if ok.contains(&reply.code) {
            Ok(reply)
        } else {
            Err(reply.into_error())
        }
    }

    /// Passive mode: EPSV, falling back to PASV. The server's advertised IP
    /// is ignored in favour of the control peer (NAT-safe).
    async fn open_data(&mut self) -> Result<TcpStream> {
        let mut port = None;
        if self.epsv {
            let reply = self.command("EPSV").await?;
            if reply.code == 229 {
                port = parse_epsv(&reply.text);
            } else {
                self.epsv = false;
            }
        }
        let port = match port {
            Some(port) => port,
            None => {
                let reply = self.expect("PASV", &[227]).await?;
                parse_pasv(&reply.text)
                    .ok_or_else(|| Error::Protocol(format!("bad PASV reply: {}", reply.text)))?
            }
        };
        dial(&self.peer.to_string(), port).await
    }

    async fn start_data(&mut self, command: &str) -> Result<Stream> {
        let tcp = self.open_data().await?;
        let reply = self.command(command).await?;
        if reply.code != 125 && reply.code != 150 {
            return Err(reply.into_error());
        }
        Ok(match &self.tls {
            Some(ctx) => Stream::Tls(Box::new(ctx.handshake(tcp).await?)),
            None => Stream::Plain(tcp),
        })
    }

    async fn end_data(&mut self) -> Result<()> {
        let reply = self.read_reply().await?;
        if reply.code == 226 || reply.code == 250 {
            Ok(())
        } else {
            Err(reply.into_error())
        }
    }

    async fn read_listing(&mut self, command: &str) -> Result<String> {
        let mut data = self.start_data(command).await?;
        let mut raw = Vec::new();
        match data.read_to_end(&mut raw).await {
            Ok(_) => {}
            // Some servers close the TLS data channel without close_notify.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e.into()),
        }
        drop(data);
        self.end_data().await?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }

    async fn list(&mut self, path: &str) -> Result<Vec<FtpEntry>> {
        if self.mlsd {
            match self.read_listing(&format!("MLSD {path}")).await {
                Ok(text) => return Ok(parse_mlsd(&text)),
                Err(Error::Reply { code, .. }) if (500..=504).contains(&code) => self.mlsd = false,
                Err(err) => return Err(err),
            }
        }
        // `-a` shows dot files on Unix servers; servers that reject it get plain LIST.
        let text = match self.read_listing(&format!("LIST -a {path}")).await {
            Err(Error::Reply { code, .. }) if code >= 500 => {
                self.read_listing(&format!("LIST {path}")).await?
            }
            other => other?,
        };
        Ok(parse_list(&text, chrono::Utc::now()))
    }
}

fn parse_pasv(text: &str) -> Option<u16> {
    let numbers: Vec<u16> = text
        .split(|c: char| !c.is_ascii_digit() && c != ',')
        .find(|t| t.matches(',').count() == 5)?
        .split(',')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    Some(numbers[4] * 256 + numbers[5])
}

fn parse_epsv(text: &str) -> Option<u16> {
    let start = text.find("|||")? + 3;
    let rest = &text[start..];
    rest[..rest.find('|')?].parse().ok()
}

/// `257 "/a ""quoted"" dir" is current directory`
fn parse_pwd(text: &str) -> Option<String> {
    let rest = &text[text.find('"')? + 1..];
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
        } else if chars.peek() == Some(&'"') {
            out.push('"');
            chars.next();
        } else {
            return Some(out);
        }
    }
    None
}

pub fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

async fn copy_with_progress<R, W>(reader: &mut R, writer: &mut W, progress: TransferProgress<'_>) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Ok(total);
        }
        writer.write_all(&buf[..n]).await?;
        total += n as u64;
        if !progress(n as u64) {
            return Err(Error::Cancelled);
        }
    }
}

/// An aborted or broken transfer leaves the control channel mid-reply; drop it
/// so the next operation reconnects.
fn settle<T>(control: &mut Option<Control>, result: &Result<T>) {
    if result
        .as_ref()
        .is_err_and(|e| e.is_connection_lost() || matches!(e, Error::Cancelled))
    {
        *control = None;
    }
}

type OpFuture<'c, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'c>>;

pub struct FtpClient {
    shared: Arc<Shared>,
    control: Mutex<Option<Control>>,
}

impl FtpClient {
    pub async fn connect(config: FtpClientConfig) -> Result<Self> {
        let tls = match config.security {
            FtpSecurity::Plain => None,
            _ => Some(client_config(
                &config.host,
                config.port,
                config.known_certs_path.clone(),
                config.prompt.clone(),
            )?),
        };
        let shared = Arc::new(Shared {
            config,
            tls,
            password: StdMutex::new(None),
            secure: AtomicBool::new(false),
        });
        Self::open(shared).await
    }

    async fn open(shared: Arc<Shared>) -> Result<Self> {
        let control = Control::connect(&shared).await?;
        Ok(Self {
            shared,
            control: Mutex::new(Some(control)),
        })
    }

    /// A second logged-in connection (same credentials and trusted
    /// certificate) for a transfer that should not block this one.
    pub async fn fork(&self) -> Result<Self> {
        Self::open(self.shared.clone()).await
    }

    /// Whether the control channel is TLS-protected.
    pub fn is_secure(&self) -> bool {
        self.shared.secure.load(Ordering::Relaxed)
    }

    /// Run `op` on the control connection, reconnecting once if the server
    /// dropped it (idle timeouts are common).
    async fn run<T, F>(&self, op: F) -> Result<T>
    where
        F: for<'c> Fn(&'c mut Control) -> OpFuture<'c, T>,
    {
        let mut guard = self.control.lock().await;
        let mut fresh = false;
        loop {
            if guard.is_none() {
                *guard = Some(Control::connect(&self.shared).await?);
                fresh = true;
            }
            let Some(control) = guard.as_mut() else {
                continue;
            };
            match op(control).await {
                Err(err) if err.is_connection_lost() => {
                    *guard = None;
                    if fresh {
                        return Err(err);
                    }
                }
                other => return other,
            }
        }
    }

    pub async fn pwd(&self) -> Result<String> {
        self.run(|c| {
            Box::pin(async move {
                let reply = c.expect("PWD", &[257]).await?;
                parse_pwd(&reply.text)
                    .ok_or_else(|| Error::Protocol(format!("bad PWD reply: {}", reply.text)))
            })
        })
        .await
    }

    pub async fn list(&self, path: &str) -> Result<Vec<FtpEntry>> {
        let path = path.to_string();
        self.run(move |c| {
            let path = path.clone();
            Box::pin(async move { c.list(&path).await })
        })
        .await
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        let command = format!("MKD {path}");
        self.run(move |c| {
            let command = command.clone();
            Box::pin(async move { c.expect(&command, &[257, 250]).await.map(drop) })
        })
        .await
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (format!("RNFR {from}"), format!("RNTO {to}"));
        self.run(move |c| {
            let (from, to) = (from.clone(), to.clone());
            Box::pin(async move {
                c.expect(&from, &[350]).await?;
                c.expect(&to, &[250]).await.map(drop)
            })
        })
        .await
    }

    async fn simple(&self, command: String, ok: &'static [u16]) -> Result<()> {
        self.run(move |c| {
            let command = command.clone();
            Box::pin(async move { c.expect(&command, ok).await.map(drop) })
        })
        .await
    }

    /// Delete a file, or a folder with everything inside it.
    pub async fn remove(&self, path: &str, is_dir: bool) -> Result<()> {
        if !is_dir {
            return self.simple(format!("DELE {path}"), &[250]).await;
        }
        for entry in self.list(path).await? {
            let child = join_path(path, &entry.name);
            Box::pin(self.remove(&child, entry.is_dir)).await?;
        }
        self.simple(format!("RMD {path}"), &[250]).await
    }

    /// `SITE CHMOD`; servers that don't support it answer 5xx.
    pub async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        self.simple(format!("SITE CHMOD {:o} {path}", mode & 0o7777), &[200, 250])
            .await
    }

    /// Create (or truncate) an empty file.
    pub async fn create_file(&self, remote: &str) -> Result<()> {
        let mut guard = self.lock_connected().await?;
        let Some(c) = guard.as_mut() else {
            return Err(Error::Closed);
        };
        let result = async {
            let mut data = c.start_data(&format!("STOR {remote}")).await?;
            data.shutdown().await?;
            drop(data);
            c.end_data().await
        }
        .await;
        settle(&mut guard, &result);
        result
    }

    /// The control connection, reconnected if a previous operation dropped it.
    async fn lock_connected(&self) -> Result<MutexGuard<'_, Option<Control>>> {
        let mut guard = self.control.lock().await;
        if guard.is_none() {
            *guard = Some(Control::connect(&self.shared).await?);
        }
        Ok(guard)
    }

    pub async fn download_with_progress(
        &self,
        remote: &str,
        local: &Path,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(local).await?;
        let mut guard = self.lock_connected().await?;
        let Some(c) = guard.as_mut() else {
            return Err(Error::Closed);
        };
        let result = async {
            let mut data = c.start_data(&format!("RETR {remote}")).await?;
            let n = copy_with_progress(&mut data, &mut file, progress).await?;
            drop(data);
            c.end_data().await?;
            file.flush().await?;
            Ok(n)
        }
        .await;
        settle(&mut guard, &result);
        result
    }

    pub async fn upload_with_progress(
        &self,
        local: &Path,
        remote: &str,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        let mut file = tokio::fs::File::open(local).await?;
        let mut guard = self.lock_connected().await?;
        let Some(c) = guard.as_mut() else {
            return Err(Error::Closed);
        };
        let result = async {
            let mut data = c.start_data(&format!("STOR {remote}")).await?;
            let n = copy_with_progress(&mut file, &mut data, progress).await?;
            data.shutdown().await?;
            drop(data);
            c.end_data().await?;
            Ok(n)
        }
        .await;
        settle(&mut guard, &result);
        result
    }

    pub async fn download_dir_with_progress(
        &self,
        remote: &str,
        local: &Path,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        tokio::fs::create_dir_all(local).await?;
        let mut total = 0;
        for entry in self.list(remote).await? {
            let remote_child = join_path(remote, &entry.name);
            let local_child = local.join(&entry.name);
            total += if entry.is_dir {
                Box::pin(self.download_dir_with_progress(&remote_child, &local_child, &mut *progress))
                    .await?
            } else {
                self.download_with_progress(&remote_child, &local_child, &mut *progress)
                    .await?
            };
        }
        Ok(total)
    }

    pub async fn upload_dir_with_progress(
        &self,
        local: &Path,
        remote: &str,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        if let Err(err) = self.mkdir(remote).await {
            // Usually "already exists"; anything worse surfaces on the first upload.
            tracing::debug!(%err, "ftp mkdir {remote}");
        }
        let mut total = 0;
        let mut rd = tokio::fs::read_dir(local).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let remote_child = join_path(remote, &name);
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                total += Box::pin(self.upload_dir_with_progress(&entry.path(), &remote_child, &mut *progress))
                    .await?;
            } else if ft.is_file() {
                total += self
                    .upload_with_progress(&entry.path(), &remote_child, &mut *progress)
                    .await?;
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::{join_path, parse_epsv, parse_pasv, parse_pwd};

    #[test]
    fn passive_replies() {
        assert_eq!(parse_pasv("Entering Passive Mode (192,168,1,2,19,137)."), Some(19 * 256 + 137));
        assert_eq!(parse_pasv("=192,168,1,2,4,1"), Some(1025));
        assert_eq!(parse_epsv("Entering Extended Passive Mode (|||6446|)"), Some(6446));
        assert_eq!(parse_epsv("garbage"), None);
    }

    #[test]
    fn pwd_and_paths() {
        assert_eq!(parse_pwd(r#""/home/ftp" is current"#).as_deref(), Some("/home/ftp"));
        assert_eq!(parse_pwd(r#""/a ""b"" c""#).as_deref(), Some(r#"/a "b" c"#));
        assert_eq!(join_path("/", "x"), "/x");
        assert_eq!(join_path("/srv", "x"), "/srv/x");
    }
}
