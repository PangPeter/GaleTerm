//! SCP over SSH (`exec scp -f/-t`), OpenSSH wire format.

use std::path::{Path, PathBuf};

use russh::ChannelMsg;
use tokio::fs::{self, File as LocalFile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::client::{connect_authenticated, Handler, SshSessionConfig};
use crate::error::{Error, Result};
use crate::sftp::default_local_download_path;

/// SCP client (keeps the SSH handle alive for the transfer).
pub struct ScpClient {
    handle: russh::client::Handle<Handler>,
    _jumps: Vec<russh::client::Handle<Handler>>,
}

impl ScpClient {
    pub async fn connect(config: SshSessionConfig) -> Result<Self> {
        let (handle, jumps) = connect_authenticated(config, None).await?;
        Ok(Self {
            handle,
            _jumps: jumps,
        })
    }

    pub async fn download(&self, remote: &str, local: impl AsRef<Path>) -> Result<u64> {
        validate_remote_path(remote)?;
        let local = local.as_ref();
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent).await?;
        }
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(format!("scp channel: {e}")))?;
        let cmd = format!("scp -f -- {}", shell_quote(remote));
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| Error::Ssh(format!("scp exec: {e}")))?;

        // Sink starts the transfer by sending a null ACK.
        channel
            .data(&[0u8][..])
            .await
            .map_err(|e| Error::Ssh(format!("scp ack: {e}")))?;

        let mut stream = ScpStream::new(channel);
        let n = recv_file(&mut stream, local).await?;
        Ok(n)
    }

    pub async fn download_dir(&self, remote: &str, local: impl AsRef<Path>) -> Result<u64> {
        validate_remote_path(remote)?;
        let local = local.as_ref();
        fs::create_dir_all(local).await?;
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(format!("scp channel: {e}")))?;
        let cmd = format!("scp -rf -- {}", shell_quote(remote));
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| Error::Ssh(format!("scp exec: {e}")))?;
        channel
            .data(&[0u8][..])
            .await
            .map_err(|e| Error::Ssh(format!("scp ack: {e}")))?;

        let mut stream = ScpStream::new(channel);
        let n = recv_tree(&mut stream, local).await?;
        Ok(n)
    }

    pub async fn upload(&self, local: impl AsRef<Path>, remote: &str) -> Result<u64> {
        validate_remote_path(remote)?;
        let local = local.as_ref();
        let meta = fs::metadata(local).await?;
        if meta.is_dir() {
            return Err(Error::Ssh(
                "local path is a directory; use recursive upload".into(),
            ));
        }
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(format!("scp channel: {e}")))?;
        let cmd = format!("scp -t -- {}", shell_quote(remote));
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| Error::Ssh(format!("scp exec: {e}")))?;

        let mut stream = ScpStream::new(channel);
        stream.expect_ack().await?;
        let name = local
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let n = send_file(&mut stream, local, &name, meta.len()).await?;
        Ok(n)
    }

    pub async fn upload_dir(&self, local: impl AsRef<Path>, remote: &str) -> Result<u64> {
        validate_remote_path(remote)?;
        let local = local.as_ref();
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(format!("scp channel: {e}")))?;
        let cmd = format!("scp -rt -- {}", shell_quote(remote));
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| Error::Ssh(format!("scp exec: {e}")))?;

        let mut stream = ScpStream::new(channel);
        stream.expect_ack().await?;
        let name = local
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dir".into());
        let n = send_tree(&mut stream, local, &name).await?;
        Ok(n)
    }
}

struct ScpStream {
    channel: russh::Channel<russh::client::Msg>,
    buf: Vec<u8>,
}

impl ScpStream {
    fn new(channel: russh::Channel<russh::client::Msg>) -> Self {
        Self {
            channel,
            buf: Vec::new(),
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.channel
            .data(data)
            .await
            .map_err(|e| Error::Ssh(format!("scp write: {e}")))
    }

    async fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut filled = 0;
        while filled < out.len() {
            if self.buf.is_empty() {
                self.fill().await?;
            }
            let n = (out.len() - filled).min(self.buf.len());
            out[filled..filled + n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            filled += n;
        }
        Ok(())
    }

    async fn read_until_newline(&mut self) -> Result<String> {
        let mut line = Vec::new();
        loop {
            if self.buf.is_empty() {
                self.fill().await?;
            }
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                line.extend_from_slice(&self.buf[..=pos]);
                self.buf.drain(..=pos);
                break;
            }
            line.extend_from_slice(&self.buf);
            self.buf.clear();
        }
        String::from_utf8(line).map_err(|e| Error::Ssh(format!("scp header utf8: {e}")))
    }

    async fn fill(&mut self) -> Result<()> {
        loop {
            match self.channel.wait().await {
                Some(ChannelMsg::Data { ref data }) => {
                    self.buf.extend_from_slice(data);
                    return Ok(());
                }
                Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    let msg = String::from_utf8_lossy(data);
                    return Err(Error::Ssh(format!("scp stderr: {msg}")));
                }
                Some(ChannelMsg::Eof) | None => {
                    return Err(Error::Ssh("scp: unexpected eof".into()));
                }
                Some(ChannelMsg::ExitStatus { exit_status }) if exit_status != 0 => {
                    return Err(Error::Ssh(format!("scp exit status {exit_status}")));
                }
                _ => continue,
            }
        }
    }

    async fn expect_ack(&mut self) -> Result<()> {
        let mut b = [0u8; 1];
        self.read_exact(&mut b).await?;
        match b[0] {
            0 => Ok(()),
            1 | 2 => {
                let msg = self.read_until_newline().await?;
                Err(Error::Ssh(format!("scp error: {}", msg.trim())))
            }
            other => Err(Error::Ssh(format!("scp: unexpected ack byte {other}"))),
        }
    }

    async fn send_ack(&mut self) -> Result<()> {
        self.write_all(&[0]).await
    }
}

async fn recv_file(stream: &mut ScpStream, local: &Path) -> Result<u64> {
    let header = read_scp_control(stream).await?;
    if header.starts_with('T') {
        // time header — ack and read the next (C/D) header
        stream.send_ack().await?;
        return Box::pin(recv_file(stream, local)).await;
    }
    if !header.starts_with('C') {
        return Err(Error::Ssh(format!("scp: expected C header, got {header}")));
    }
    let (_mode, size, _name) = parse_c_header(&header)?;
    stream.send_ack().await?;

    let mut file = LocalFile::create(local).await?;
    let mut remaining = size;
    let mut chunk = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let n = remaining.min(chunk.len() as u64) as usize;
        stream.read_exact(&mut chunk[..n]).await?;
        file.write_all(&chunk[..n]).await?;
        remaining -= n as u64;
    }
    stream.expect_ack().await?; // trailing null from remote
    stream.send_ack().await?;
    Ok(size)
}

async fn read_scp_control(stream: &mut ScpStream) -> Result<String> {
    let mut first = [0u8; 1];
    stream.read_exact(&mut first).await?;
    match first[0] {
        1 | 2 => {
            let msg = stream.read_until_newline().await?;
            Err(Error::Ssh(format!("scp error: {}", msg.trim())))
        }
        b'T' | b'C' | b'D' | b'E' => {
            let rest = stream.read_until_newline().await?;
            Ok(format!("{}{}", first[0] as char, rest.trim_end_matches(['\n', '\r'])))
        }
        other => Err(Error::Ssh(format!(
            "scp: unexpected control byte {other:#x}"
        ))),
    }
}

async fn recv_tree(stream: &mut ScpStream, local_dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![local_dir.to_path_buf()];

    loop {
        let header = read_scp_control(stream).await?;
        if header.starts_with('T') {
            stream.send_ack().await?;
            continue;
        }
        if header == "E" {
            stream.send_ack().await?;
            stack.pop();
            if stack.is_empty() {
                break;
            }
            continue;
        }
        if header.starts_with('D') {
            let (_mode, _size, name) = parse_c_header(&header)?;
            let parent = stack.last().cloned().unwrap_or_else(|| local_dir.to_path_buf());
            let dir = parent.join(&name);
            fs::create_dir_all(&dir).await?;
            stack.push(dir);
            stream.send_ack().await?;
            continue;
        }
        if header.starts_with('C') {
            let (_mode, size, name) = parse_c_header(&header)?;
            let parent = stack.last().cloned().unwrap_or_else(|| local_dir.to_path_buf());
            let path = parent.join(&name);
            stream.send_ack().await?;
            let mut file = LocalFile::create(&path).await?;
            let mut remaining = size;
            let mut chunk = vec![0u8; 32 * 1024];
            while remaining > 0 {
                let n = remaining.min(chunk.len() as u64) as usize;
                stream.read_exact(&mut chunk[..n]).await?;
                file.write_all(&chunk[..n]).await?;
                remaining -= n as u64;
            }
            stream.expect_ack().await?;
            stream.send_ack().await?;
            total += size;
            continue;
        }
        return Err(Error::Ssh(format!("scp: unexpected header {header}")));
    }
    Ok(total)
}

async fn send_file(stream: &mut ScpStream, local: &Path, name: &str, size: u64) -> Result<u64> {
    let mode = unix_mode(local).await.unwrap_or(0o644);
    let header = format!("C{mode:04o} {size} {name}\n");
    stream.write_all(header.as_bytes()).await?;
    stream.expect_ack().await?;

    let mut file = LocalFile::open(local).await?;
    let mut remaining = size;
    let mut chunk = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let n = remaining.min(chunk.len() as u64) as usize;
        file.read_exact(&mut chunk[..n]).await?;
        stream.write_all(&chunk[..n]).await?;
        remaining -= n as u64;
    }
    stream.write_all(&[0]).await?;
    stream.expect_ack().await?;
    Ok(size)
}

async fn send_tree(stream: &mut ScpStream, local: &Path, name: &str) -> Result<u64> {
    let mode = unix_mode(local).await.unwrap_or(0o755);
    let header = format!("D{mode:04o} 0 {name}\n");
    stream.write_all(header.as_bytes()).await?;
    stream.expect_ack().await?;

    let mut total = 0u64;
    let mut rd = fs::read_dir(local).await?;
    while let Some(entry) = rd.next_entry().await? {
        let child_name = entry.file_name();
        let child_name = child_name.to_string_lossy();
        let path = entry.path();
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            total += Box::pin(send_tree(stream, &path, &child_name)).await?;
        } else if ft.is_file() {
            let size = fs::metadata(&path).await?.len();
            total += send_file(stream, &path, &child_name, size).await?;
        }
    }

    stream.write_all(b"E\n").await?;
    stream.expect_ack().await?;
    Ok(total)
}

fn parse_c_header(header: &str) -> Result<(u32, u64, String)> {
    // C0644 1234 name  or D0755 0 name
    let rest = &header[1..];
    let mut parts = rest.splitn(3, ' ');
    let mode_s = parts
        .next()
        .ok_or_else(|| Error::Ssh(format!("scp bad header: {header}")))?;
    let size_s = parts
        .next()
        .ok_or_else(|| Error::Ssh(format!("scp bad header: {header}")))?;
    let name = parts
        .next()
        .ok_or_else(|| Error::Ssh(format!("scp bad header: {header}")))?
        .trim()
        .to_string();
    let mode = u32::from_str_radix(mode_s, 8)
        .map_err(|e| Error::Ssh(format!("scp mode: {e}")))?;
    let size = size_s
        .parse::<u64>()
        .map_err(|e| Error::Ssh(format!("scp size: {e}")))?;
    if name.contains('/') || name == ".." || name.is_empty() {
        return Err(Error::Ssh(format!("scp: unsafe name `{name}`")));
    }
    Ok((mode, size, name))
}

async fn unix_mode(path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path).await?;
        Ok(meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(0o644)
    }
}

fn validate_remote_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::Ssh("empty remote path".into()));
    }
    if path.contains('\n') || path.contains('\r') || path.contains('\0') {
        return Err(Error::Ssh("remote path contains illegal characters".into()));
    }
    Ok(())
}

/// Single-quote for remote shell, with `'\''` escaping.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Resolve local download destination (re-export helper for CLI).
pub fn default_local_scp_path(remote: &str, local: Option<&str>) -> PathBuf {
    default_local_download_path(remote, local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn parse_header() {
        let (mode, size, name) = parse_c_header("C0644 12 foo.txt").unwrap();
        assert_eq!(mode, 0o644);
        assert_eq!(size, 12);
        assert_eq!(name, "foo.txt");
    }

    #[test]
    fn rejects_path_traversal_name() {
        assert!(parse_c_header("C0644 1 ../x").is_err());
    }

    #[test]
    fn default_local_path() {
        assert_eq!(
            default_local_scp_path("/tmp/a.txt", None),
            PathBuf::from("a.txt")
        );
    }
}
