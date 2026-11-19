//! Outbound proxies for the initial SSH TCP connection (HTTP CONNECT / SOCKS5).

use std::time::Duration;

use gale_core::{ProxyConfig, ProxyKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

/// Parse `scheme://[user[:pass]@]host:port`
///
/// Schemes: `http`, `https` (HTTP CONNECT), `socks5`, `socks`.
pub fn parse_proxy_url(url: &str) -> Result<ProxyConfig> {
    let url = url.trim();
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::Ssh(format!("invalid proxy URL `{url}` (missing scheme://)")))?;
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => ProxyKind::Http,
        "socks5" | "socks" => ProxyKind::Socks5,
        other => {
            return Err(Error::Ssh(format!(
                "unsupported proxy scheme `{other}` (http|socks5)"
            )))
        }
    };

    let (userinfo, hostport) = match rest.split_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };

    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (host, port_part) = rest.split_once("]:").ok_or_else(|| {
            Error::Ssh(format!("invalid proxy URL `{url}` (expected [ipv6]:port)"))
        })?;
        let port: u16 = port_part
            .parse()
            .map_err(|_| Error::Ssh(format!("invalid proxy port in `{url}`")))?;
        (host.to_string(), port)
    } else {
        let (host, port) = hostport.rsplit_once(':').ok_or_else(|| {
            Error::Ssh(format!("invalid proxy URL `{url}` (expected host:port)"))
        })?;
        let port: u16 = port
            .parse()
            .map_err(|_| Error::Ssh(format!("invalid proxy port in `{url}`")))?;
        (host.to_string(), port)
    };

    let (username, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (Some(percent_decode(u)), Some(percent_decode(p))),
            None => (Some(percent_decode(ui)), None),
        },
        None => (None, None),
    };

    Ok(ProxyConfig {
        kind,
        host,
        port,
        username,
        password,
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Dial `target_host:target_port` via the given outbound proxy.
pub async fn dial_via_proxy(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|e| {
            Error::Ssh(format!(
                "failed to connect to proxy {}:{}: {e}",
                proxy.host, proxy.port
            ))
        })?;
    stream.set_nodelay(true).ok();

    match proxy.kind {
        ProxyKind::Http => http_connect(&mut stream, proxy, target_host, target_port).await?,
        ProxyKind::Socks5 => socks5_connect(&mut stream, proxy, target_host, target_port).await?,
    }
    Ok(stream)
}

async fn http_connect(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<()> {
    let authority = format_authority(target_host, target_port);
    let mut req = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
    );
    if let Some(user) = &proxy.username {
        let pass = proxy.password.as_deref().unwrap_or("");
        let token = base64_encode(format!("{user}:{pass}").as_bytes());
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;

    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 256];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(Error::Ssh("proxy CONNECT timed out".into()));
        }
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .map_err(|_| Error::Ssh("proxy CONNECT timed out".into()))?
            .map_err(|e| Error::Ssh(format!("proxy CONNECT read error: {e}")))?;
        if n == 0 {
            return Err(Error::Ssh("proxy closed during CONNECT".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(Error::Ssh("proxy CONNECT response too large".into()));
        }
    }

    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap();
    let header = String::from_utf8_lossy(&buf[..header_end]);
    let status_line = header.lines().next().unwrap_or("");
    let ok = status_line.split_whitespace().nth(1) == Some("200");
    if !ok {
        return Err(Error::Ssh(format!("proxy CONNECT failed: {status_line}")));
    }
    if header_end < buf.len() {
        return Err(Error::Ssh(
            "unexpected data after proxy CONNECT response".into(),
        ));
    }
    Ok(())
}

async fn socks5_connect(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<()> {
    if proxy.username.is_some() {
        stream.write_all(&[0x05, 0x02, 0x00, 0x02]).await?;
    } else {
        stream.write_all(&[0x05, 0x01, 0x00]).await?;
    }
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    if buf[0] != 0x05 {
        return Err(Error::Ssh(format!(
            "SOCKS version mismatch from proxy: {}",
            buf[0]
        )));
    }
    match buf[1] {
        0x00 => {}
        0x02 => {
            let user = proxy
                .username
                .as_deref()
                .ok_or_else(|| Error::Ssh("SOCKS5 proxy requested username/password".into()))?;
            let pass = proxy.password.as_deref().unwrap_or("");
            if user.len() > 255 || pass.len() > 255 {
                return Err(Error::Ssh("SOCKS5 credentials too long".into()));
            }
            let mut auth = Vec::with_capacity(3 + user.len() + pass.len());
            auth.push(0x01);
            auth.push(user.len() as u8);
            auth.extend_from_slice(user.as_bytes());
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass.as_bytes());
            stream.write_all(&auth).await?;
            let mut resp = [0u8; 2];
            stream.read_exact(&mut resp).await?;
            if resp[1] != 0x00 {
                return Err(Error::Ssh("SOCKS5 proxy authentication failed".into()));
            }
        }
        0xFF => return Err(Error::Ssh("SOCKS5 proxy rejected auth methods".into())),
        other => {
            return Err(Error::Ssh(format!(
                "SOCKS5 proxy selected unsupported method {other}"
            )))
        }
    }

    let mut req = vec![0x05, 0x01, 0x00];
    if let Ok(ip) = target_host.parse::<std::net::Ipv4Addr>() {
        req.push(0x01);
        req.extend_from_slice(&ip.octets());
    } else if let Ok(ip) = target_host.parse::<std::net::Ipv6Addr>() {
        req.push(0x04);
        req.extend_from_slice(&ip.octets());
    } else {
        let host_bytes = target_host.as_bytes();
        if host_bytes.len() > 255 {
            return Err(Error::Ssh("SOCKS5 domain name too long".into()));
        }
        req.push(0x03);
        req.push(host_bytes.len() as u8);
        req.extend_from_slice(host_bytes);
    }
    req.extend_from_slice(&target_port.to_be_bytes());
    stream.write_all(&req).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(Error::Ssh("invalid SOCKS5 reply version".into()));
    }
    if head[1] != 0x00 {
        return Err(Error::Ssh(format!(
            "SOCKS5 CONNECT failed with code {}",
            head[1]
        )));
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x03 => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await?;
            l[0] as usize
        }
        0x04 => 16,
        other => {
            return Err(Error::Ssh(format!(
                "SOCKS5 reply has unknown atyp {other}"
            )))
        }
    };
    let mut skip = vec![0u8; addr_len + 2];
    stream.read_exact(&mut skip).await?;
    Ok(())
}

fn format_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let mut n = (chunk[0] as u32) << 16;
        if chunk.len() > 1 {
            n |= (chunk[1] as u32) << 8;
        }
        if chunk.len() > 2 {
            n |= chunk[2] as u32;
        }
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parse_socks_url() {
        let p = parse_proxy_url("socks5://127.0.0.1:1080").unwrap();
        assert_eq!(p.kind, ProxyKind::Socks5);
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 1080);
        assert!(p.username.is_none());
    }

    #[test]
    fn parse_http_url_with_auth() {
        let p = parse_proxy_url("http://alice:s%3Bcret@proxy.example:8080").unwrap();
        assert_eq!(p.kind, ProxyKind::Http);
        assert_eq!(p.host, "proxy.example");
        assert_eq!(p.port, 8080);
        assert_eq!(p.username.as_deref(), Some("alice"));
        assert_eq!(p.password.as_deref(), Some("s;cret"));
    }

    #[test]
    fn base64_basic() {
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[tokio::test]
    async fn http_connect_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("CONNECT example.com:22 "));
            assert!(req.contains("Proxy-Authorization: Basic "));
            s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
        });

        let proxy = ProxyConfig {
            kind: ProxyKind::Http,
            host: addr.ip().to_string(),
            port: addr.port(),
            username: Some("u".into()),
            password: Some("p".into()),
        };
        let _stream = dial_via_proxy(&proxy, "example.com", 22).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_connect_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            s.read_exact(&mut buf[..3]).await.unwrap();
            s.write_all(&[0x05, 0x00]).await.unwrap();
            s.read_exact(&mut buf[..5]).await.unwrap();
            assert_eq!(&buf[..4], &[0x05, 0x01, 0x00, 0x03]);
            let len = buf[4] as usize;
            s.read_exact(&mut buf[..len + 2]).await.unwrap();
            assert_eq!(&buf[..len], b"example.com");
            s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });

        let proxy = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: addr.ip().to_string(),
            port: addr.port(),
            username: None,
            password: None,
        };
        let _stream = dial_via_proxy(&proxy, "example.com", 22).await.unwrap();
        server.await.unwrap();
    }
}
