//! SSH port forwarding: local (-L), remote (-R), and dynamic SOCKS5 (-D).

use std::net::SocketAddr;
use std::sync::Arc;

use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::client::{connect_authenticated, IncomingRemoteForward, SshSessionConfig};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardMode {
    /// `-L [bind:]port:host:hostport`
    Local,
    /// `-R [bind:]port:host:hostport`
    Remote,
    /// `-D [bind:]port` (SOCKS5)
    Dynamic,
}

#[derive(Debug, Clone)]
pub struct ForwardSpec {
    pub mode: ForwardMode,
    /// Local or remote listen address (for Dynamic: local SOCKS bind).
    pub bind: SocketAddr,
    /// Destination for Local/Remote (ignored for Dynamic).
    pub dest_host: String,
    pub dest_port: u16,
}

impl ForwardSpec {
    pub fn local(bind: SocketAddr, dest_host: impl Into<String>, dest_port: u16) -> Self {
        Self {
            mode: ForwardMode::Local,
            bind,
            dest_host: dest_host.into(),
            dest_port,
        }
    }

    pub fn remote(bind: SocketAddr, dest_host: impl Into<String>, dest_port: u16) -> Self {
        Self {
            mode: ForwardMode::Remote,
            bind,
            dest_host: dest_host.into(),
            dest_port,
        }
    }

    pub fn dynamic(bind: SocketAddr) -> Self {
        Self {
            mode: ForwardMode::Dynamic,
            bind,
            dest_host: String::new(),
            dest_port: 0,
        }
    }
}

/// Runs until cancelled (Ctrl+C) or the SSH connection drops.
pub async fn run_forward(config: SshSessionConfig, spec: ForwardSpec) -> Result<()> {
    match spec.mode {
        ForwardMode::Local => run_local(config, spec).await,
        ForwardMode::Remote => run_remote(config, spec).await,
        ForwardMode::Dynamic => run_dynamic(config, spec).await,
    }
}

/// Programmatic local forward for embedding (VNC/RDP jump tunnels).
///
/// Binds `127.0.0.1:0`, opens SSH (with ProxyJump hops), and relays each
/// accepted TCP connection via `direct-tcpip` to `dest_host:dest_port` as seen
/// from the final SSH hop. Drop the returned value to tear everything down.
pub struct LocalTunnel {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl LocalTunnel {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn local_port(&self) -> u16 {
        self.local_addr.port()
    }
}

impl Drop for LocalTunnel {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.task.abort();
    }
}

/// Open an ephemeral local forward to `dest_host:dest_port` through `config`.
pub async fn open_local_tunnel(
    config: SshSessionConfig,
    dest_host: impl Into<String>,
    dest_port: u16,
) -> Result<LocalTunnel> {
    let dest_host = dest_host.into();
    let (handle, jumps) = connect_authenticated(config, None).await?;
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .map_err(|e| Error::Ssh(format!("failed to bind local tunnel: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| Error::Ssh(format!("local tunnel addr: {e}")))?;
    tracing::info!(
        %local_addr,
        dest = %format!("{dest_host}:{dest_port}"),
        "SSH local tunnel ready"
    );

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        // Keep jump handles alive for the tunneled direct-tcpip channels.
        let _jumps = jumps;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((socket, peer)) = accepted else { break };
                    match open_direct(&handle, &dest_host, dest_port, peer).await {
                        Ok(channel) => {
                            tokio::spawn(async move {
                                let mut socket = socket;
                                if let Err(e) = pump_tcp_and_channel(&mut socket, channel).await {
                                    tracing::debug!("local tunnel connection closed: {e}");
                                }
                            });
                        }
                        Err(e) => tracing::warn!("local tunnel open failed: {e}"),
                    }
                }
            }
        }
        // Dropping `handle` closes the SSH session.
    });

    Ok(LocalTunnel {
        local_addr,
        shutdown: Some(shutdown_tx),
        task,
    })
}

async fn run_local(config: SshSessionConfig, spec: ForwardSpec) -> Result<()> {
    let (handle, _jumps) = connect_authenticated(config, None).await?;
    let listener = TcpListener::bind(spec.bind)
        .await
        .map_err(|e| Error::Ssh(format!("failed to bind {}: {e}", spec.bind)))?;
    eprintln!(
        "local forward: {} -> {}:{} (Ctrl+C to stop)",
        spec.bind, spec.dest_host, spec.dest_port
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("stopping local forward");
                break;
            }
            accepted = listener.accept() => {
                let (socket, peer) = accepted.map_err(|e| Error::Ssh(e.to_string()))?;
                match open_direct(&handle, &spec.dest_host, spec.dest_port, peer).await {
                    Ok(channel) => {
                        tokio::spawn(async move {
                            let mut socket = socket;
                            if let Err(e) = pump_tcp_and_channel(&mut socket, channel).await {
                                tracing::debug!("local forward connection closed: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("local forward open failed: {e}"),
                }
            }
        }
    }
    Ok(())
}

async fn open_direct(
    handle: &russh::client::Handle<crate::client::Handler>,
    dest_host: &str,
    dest_port: u16,
    peer: SocketAddr,
) -> Result<Channel<Msg>> {
    handle
        .channel_open_direct_tcpip(
            dest_host.to_string(),
            u32::from(dest_port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
        .map_err(|e| Error::Ssh(format!("direct-tcpip failed: {e}")))
}

async fn run_remote(config: SshSessionConfig, spec: ForwardSpec) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<IncomingRemoteForward>();
    let (handle, _jumps) = connect_authenticated(config, Some(tx)).await?;

    let bind_host = spec.bind.ip().to_string();
    let bind_port = u32::from(spec.bind.port());
    let assigned = handle
        .tcpip_forward(bind_host.clone(), bind_port)
        .await
        .map_err(|e| Error::Ssh(format!("tcpip-forward request denied: {e}")))?;
    let shown_port = if bind_port == 0 { assigned } else { bind_port };
    eprintln!(
        "remote forward: {bind_host}:{shown_port} -> {}:{} (Ctrl+C to stop)",
        spec.dest_host, spec.dest_port
    );

    let dest_host = Arc::new(spec.dest_host.clone());
    let dest_port = spec.dest_port;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = handle.cancel_tcpip_forward(bind_host, bind_port).await;
                eprintln!("stopping remote forward");
                break;
            }
            incoming = rx.recv() => {
                let Some(incoming) = incoming else { break };
                let dest_host = Arc::clone(&dest_host);
                tokio::spawn(async move {
                    if let Err(e) = relay_remote_connection(incoming.channel, &dest_host, dest_port).await {
                        tracing::debug!("remote forward connection closed: {e}");
                    }
                });
            }
        }
    }
    Ok(())
}

async fn relay_remote_connection(
    channel: Channel<Msg>,
    dest_host: &str,
    dest_port: u16,
) -> Result<()> {
    let mut socket = TcpStream::connect((dest_host, dest_port))
        .await
        .map_err(|e| Error::Ssh(format!("connect {dest_host}:{dest_port}: {e}")))?;
    pump_tcp_and_channel(&mut socket, channel).await
}

async fn run_dynamic(config: SshSessionConfig, spec: ForwardSpec) -> Result<()> {
    let (handle, _jumps) = connect_authenticated(config, None).await?;
    let listener = TcpListener::bind(spec.bind)
        .await
        .map_err(|e| Error::Ssh(format!("failed to bind {}: {e}", spec.bind)))?;
    eprintln!("dynamic SOCKS5: {} (Ctrl+C to stop)", spec.bind);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("stopping dynamic forward");
                break;
            }
            accepted = listener.accept() => {
                let (mut socket, peer) = accepted.map_err(|e| Error::Ssh(e.to_string()))?;
                match socks5_handshake(&mut socket).await {
                    Ok((host, port)) => {
                        match open_direct(&handle, &host, port, peer).await {
                            Ok(channel) => {
                                // SOCKS5 success reply
                                if socket
                                    .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                                    .await
                                    .is_ok()
                                {
                                    tokio::spawn(async move {
                                        if let Err(e) = pump_tcp_and_channel(&mut socket, channel).await {
                                            tracing::debug!("socks connection closed: {e}");
                                        }
                                    });
                                }
                            }
                            Err(e) => {
                                let _ = socket
                                    .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                                    .await;
                                tracing::warn!("socks direct-tcpip failed: {e}");
                            }
                        }
                    }
                    Err(e) => tracing::debug!("socks handshake failed: {e}"),
                }
            }
        }
    }
    Ok(())
}

/// Minimal SOCKS5: no-auth + CONNECT (IPv4 / domain / IPv6).
pub async fn socks5_handshake(socket: &mut TcpStream) -> Result<(String, u16)> {
    let mut buf = [0u8; 512];
    socket.read_exact(&mut buf[..2]).await?;
    if buf[0] != 0x05 {
        return Err(Error::Ssh(format!("unsupported SOCKS version {}", buf[0])));
    }
    let nmethods = buf[1] as usize;
    socket.read_exact(&mut buf[..nmethods]).await?;
    // Select no-auth
    socket.write_all(&[0x05, 0x00]).await?;

    socket.read_exact(&mut buf[..4]).await?;
    if buf[0] != 0x05 || buf[1] != 0x01 {
        // Only CONNECT
        let _ = socket.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
        return Err(Error::Ssh("SOCKS5 only supports CONNECT".into()));
    }
    let atyp = buf[3];
    let host = match atyp {
        0x01 => {
            socket.read_exact(&mut buf[..4]).await?;
            format!("{}.{}.{}.{}", buf[0], buf[1], buf[2], buf[3])
        }
        0x03 => {
            socket.read_exact(&mut buf[..1]).await?;
            let len = buf[0] as usize;
            socket.read_exact(&mut buf[..len]).await?;
            String::from_utf8_lossy(&buf[..len]).into_owned()
        }
        0x04 => {
            socket.read_exact(&mut buf[..16]).await?;
            let addr = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&buf[..16]).unwrap());
            addr.to_string()
        }
        other => {
            let _ = socket.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
            return Err(Error::Ssh(format!("unsupported SOCKS atyp {other}")));
        }
    };
    socket.read_exact(&mut buf[..2]).await?;
    let port = u16::from_be_bytes([buf[0], buf[1]]);
    Ok((host, port))
}

async fn pump_tcp_and_channel(socket: &mut TcpStream, mut channel: Channel<Msg>) -> Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    let mut socket_closed = false;
    loop {
        tokio::select! {
            read = socket.read(&mut buf), if !socket_closed => {
                match read {
                    Ok(0) => {
                        socket_closed = true;
                        let _ = channel.eof().await;
                    }
                    Ok(n) => {
                        channel.data(&buf[..n]).await.map_err(|e| Error::Ssh(e.to_string()))?;
                    }
                    Err(e) => return Err(Error::Io(e)),
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { ref data }) => {
                        socket.write_all(data).await?;
                    }
                    Some(ChannelMsg::Eof) | None => {
                        if !socket_closed {
                            let _ = channel.eof().await;
                        }
                        break;
                    }
                    Some(ChannelMsg::WindowAdjusted { .. }) => {}
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Parse `host:port` or `[ipv6]:port`.
pub fn parse_host_port(s: &str) -> Result<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| Error::Ssh(format!("invalid address `{s}` (expected [ipv6]:port)")))?;
        let port: u16 = port_part
            .parse()
            .map_err(|_| Error::Ssh(format!("invalid port in `{s}`")))?;
        return Ok((host.to_string(), port));
    }
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| Error::Ssh(format!("invalid address `{s}` (expected host:port)")))?;
    let port: u16 = port
        .parse()
        .map_err(|_| Error::Ssh(format!("invalid port in `{s}`")))?;
    Ok((host.to_string(), port))
}

/// Parse bind address; bare port means `127.0.0.1:port`.
pub fn parse_bind_addr(s: &str) -> Result<SocketAddr> {
    if let Ok(port) = s.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    s.parse()
        .map_err(|e| Error::Ssh(format!("invalid bind address `{s}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bind_port_only() {
        assert_eq!(
            parse_bind_addr("1080").unwrap(),
            "127.0.0.1:1080".parse().unwrap()
        );
    }

    #[test]
    fn parse_host_port_basic() {
        assert_eq!(
            parse_host_port("10.0.0.5:80").unwrap(),
            ("10.0.0.5".into(), 80)
        );
    }

    #[test]
    fn parse_host_port_ipv6() {
        assert_eq!(
            parse_host_port("[::1]:443").unwrap(),
            ("::1".into(), 443)
        );
    }

    #[tokio::test]
    async fn socks5_handshake_domain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            // greeting: ver=5, 1 method (no-auth)
            s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut buf = [0u8; 2];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, [0x05, 0x00]);
            // CONNECT example.com:80
            let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
            req.extend_from_slice(b"example.com");
            req.extend_from_slice(&80u16.to_be_bytes());
            s.write_all(&req).await.unwrap();
            // leave connection open for server to finish read
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });

        let (mut server, _) = listener.accept().await.unwrap();
        let (host, port) = socks5_handshake(&mut server).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        let _ = client.await;
    }
}
