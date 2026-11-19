//! SFTP client built on `russh-sftp`.

use std::path::{Path, PathBuf};

use russh_sftp::client::fs::DirEntry;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::fs::File as LocalFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{connect_authenticated, Handler, SshSessionConfig};
use crate::error::{Error, Result};

/// High-level SFTP session (keeps the underlying SSH handle alive).
pub struct SftpClient {
    sftp: SftpSession,
    _handle: russh::client::Handle<Handler>,
    _jumps: Vec<russh::client::Handle<Handler>>,
}

#[derive(Debug, Clone)]
pub struct SftpListEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: Option<u64>,
    pub permissions: Option<u32>,
    /// Seconds since the Unix epoch.
    pub modified: Option<u32>,
}

/// Called with the number of bytes moved since the last call; return `false`
/// to abort the transfer with [`Error::Cancelled`].
pub type TransferProgress<'a> = &'a mut (dyn FnMut(u64) -> bool + Send);

const COPY_CHUNK: usize = 64 * 1024;

async fn copy_with_progress<R, W>(
    reader: &mut R,
    writer: &mut W,
    progress: TransferProgress<'_>,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
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

impl SftpListEntry {
    fn from_dir_entry(entry: DirEntry) -> Self {
        let ft = entry.file_type();
        let meta = entry.metadata();
        Self {
            name: entry.file_name(),
            path: entry.path(),
            is_dir: ft.is_dir(),
            is_symlink: ft.is_symlink(),
            size: meta.size,
            permissions: meta.permissions,
            modified: meta.mtime,
        }
    }

    /// Long listing style: `drwxr-xr-x  4096 name`
    pub fn format_long(&self) -> String {
        let mode = format_mode(self.permissions.unwrap_or(0), self.is_dir, self.is_symlink);
        let size = self.size.unwrap_or(0);
        format!("{mode} {size:>12} {}", self.name)
    }
}

impl SftpClient {
    pub async fn connect(config: SshSessionConfig) -> Result<Self> {
        let (handle, jumps) = connect_authenticated(config, None).await?;
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(format!("sftp channel: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| Error::Ssh(format!("sftp subsystem: {e}")))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| Error::Ssh(format!("sftp init: {e}")))?;
        Ok(Self {
            sftp,
            _handle: handle,
            _jumps: jumps,
        })
    }

    pub async fn pwd(&self) -> Result<String> {
        self.sftp
            .canonicalize(".")
            .await
            .map_err(|e| Error::Ssh(format!("sftp pwd: {e}")))
    }

    pub async fn list(&self, path: &str) -> Result<Vec<SftpListEntry>> {
        let dir = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|e| Error::Ssh(format!("sftp ls `{path}`: {e}")))?;
        let mut entries: Vec<_> = dir.map(SftpListEntry::from_dir_entry).collect();
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.sftp
            .create_dir(path)
            .await
            .map_err(|e| Error::Ssh(format!("sftp mkdir `{path}`: {e}")))
    }

    pub async fn rm(&self, path: &str, recursive: bool) -> Result<()> {
        let meta = self
            .sftp
            .metadata(path)
            .await
            .map_err(|e| Error::Ssh(format!("sftp stat `{path}`: {e}")))?;
        if meta.file_type().is_dir() {
            if recursive {
                self.rm_dir_recursive(path).await
            } else {
                self.sftp
                    .remove_dir(path)
                    .await
                    .map_err(|e| Error::Ssh(format!("sftp rmdir `{path}`: {e}")))
            }
        } else {
            self.sftp
                .remove_file(path)
                .await
                .map_err(|e| Error::Ssh(format!("sftp rm `{path}`: {e}")))
        }
    }

    async fn rm_dir_recursive(&self, path: &str) -> Result<()> {
        let entries = self.list(path).await?;
        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let child = join_remote(path, &entry.name);
            if entry.is_dir {
                Box::pin(self.rm_dir_recursive(&child)).await?;
            } else {
                self.sftp
                    .remove_file(&child)
                    .await
                    .map_err(|e| Error::Ssh(format!("sftp rm `{child}`: {e}")))?;
            }
        }
        self.sftp
            .remove_dir(path)
            .await
            .map_err(|e| Error::Ssh(format!("sftp rmdir `{path}`: {e}")))
    }

    pub async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let mut attrs = russh_sftp::protocol::FileAttributes::empty();
        attrs.permissions = Some(mode & 0o7777);
        self.sftp
            .set_metadata(path, attrs)
            .await
            .map_err(|e| Error::Ssh(format!("sftp chmod `{path}`: {e}")))
    }

    /// Create (or truncate) an empty file.
    pub async fn create_file(&self, path: &str) -> Result<()> {
        let mut file = self
            .sftp
            .create(path)
            .await
            .map_err(|e| Error::Ssh(format!("sftp create `{path}`: {e}")))?;
        file.shutdown().await?;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.sftp
            .rename(from, to)
            .await
            .map_err(|e| Error::Ssh(format!("sftp mv `{from}` -> `{to}`: {e}")))
    }

    pub async fn download(&self, remote: &str, local: impl AsRef<Path>) -> Result<u64> {
        self.download_with_progress(remote, local.as_ref(), &mut |_| true)
            .await
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
        let mut remote_file = self
            .sftp
            .open(remote)
            .await
            .map_err(|e| Error::Ssh(format!("sftp open `{remote}`: {e}")))?;
        let mut local_file = LocalFile::create(local).await?;
        let n = copy_with_progress(&mut remote_file, &mut local_file, progress).await?;
        local_file.flush().await?;
        remote_file
            .shutdown()
            .await
            .map_err(|e| Error::Ssh(format!("sftp close `{remote}`: {e}")))?;
        Ok(n)
    }

    pub async fn upload(&self, local: impl AsRef<Path>, remote: &str) -> Result<u64> {
        self.upload_with_progress(local.as_ref(), remote, &mut |_| true)
            .await
    }

    pub async fn upload_with_progress(
        &self,
        local: &Path,
        remote: &str,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        let mut local_file = LocalFile::open(local).await.map_err(|e| {
            Error::Ssh(format!("open local `{}`: {e}", local.display()))
        })?;
        let mut remote_file = self
            .sftp
            .open_with_flags(
                remote,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| Error::Ssh(format!("sftp create `{remote}`: {e}")))?;
        let n = copy_with_progress(&mut local_file, &mut remote_file, progress).await?;
        remote_file
            .sync_all()
            .await
            .map_err(|e| Error::Ssh(format!("sftp sync `{remote}`: {e}")))?;
        remote_file
            .shutdown()
            .await
            .map_err(|e| Error::Ssh(format!("sftp close `{remote}`: {e}")))?;
        Ok(n)
    }

    /// Download a remote directory tree into `local` (created if needed).
    pub async fn download_dir(&self, remote: &str, local: impl AsRef<Path>) -> Result<u64> {
        self.download_dir_with_progress(remote, local.as_ref(), &mut |_| true)
            .await
    }

    pub async fn download_dir_with_progress(
        &self,
        remote: &str,
        local: &Path,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        tokio::fs::create_dir_all(local).await?;
        let mut total = 0u64;
        for entry in self.list(remote).await? {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let remote_child = join_remote(remote, &entry.name);
            let local_child = local.join(&entry.name);
            if entry.is_dir {
                total += Box::pin(self.download_dir_with_progress(
                    &remote_child,
                    &local_child,
                    &mut *progress,
                ))
                .await?;
            } else {
                total += self
                    .download_with_progress(&remote_child, &local_child, &mut *progress)
                    .await?;
            }
        }
        Ok(total)
    }

    /// Upload a local directory tree to `remote` (created if needed).
    pub async fn upload_dir(&self, local: impl AsRef<Path>, remote: &str) -> Result<u64> {
        self.upload_dir_with_progress(local.as_ref(), remote, &mut |_| true)
            .await
    }

    pub async fn upload_dir_with_progress(
        &self,
        local: &Path,
        remote: &str,
        progress: TransferProgress<'_>,
    ) -> Result<u64> {
        let _ = self.sftp.create_dir(remote).await; // ok if exists
        let mut total = 0u64;
        let mut rd = tokio::fs::read_dir(local).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let local_child = entry.path();
            let remote_child = join_remote(remote, &name);
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                total += Box::pin(self.upload_dir_with_progress(
                    &local_child,
                    &remote_child,
                    &mut *progress,
                ))
                .await?;
            } else if ft.is_file() {
                total += self
                    .upload_with_progress(&local_child, &remote_child, &mut *progress)
                    .await?;
            }
        }
        Ok(total)
    }
}

pub fn join_remote(parent: &str, name: &str) -> String {
    if parent.is_empty() || parent == "." {
        return name.to_string();
    }
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn format_mode(perms: u32, is_dir: bool, is_symlink: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_dir {
        'd'
    } else if is_symlink {
        'l'
    } else {
        '-'
    });
    for shift in [6, 3, 0] {
        let bits = (perms >> shift) & 0o7;
        s.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        s.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        s.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Resolve local path for downloads when only a directory is given.
pub fn default_local_download_path(remote: &str, local: Option<&str>) -> PathBuf {
    let remote_name = Path::new(remote)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| remote.to_string());
    match local {
        Some(p) => {
            let as_dir = p.ends_with('/') || p.ends_with('\\') || Path::new(p).is_dir();
            if as_dir {
                PathBuf::from(p).join(remote_name)
            } else {
                PathBuf::from(p)
            }
        }
        None => PathBuf::from(remote_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_remote_paths() {
        assert_eq!(join_remote("/tmp", "a"), "/tmp/a");
        assert_eq!(join_remote("/tmp/", "a"), "/tmp/a");
        assert_eq!(join_remote(".", "a"), "a");
    }

    #[test]
    fn mode_formatting() {
        assert_eq!(format_mode(0o755, true, false), "drwxr-xr-x");
        assert_eq!(format_mode(0o644, false, false), "-rw-r--r--");
        assert_eq!(format_mode(0o777, false, true), "lrwxrwxrwx");
    }

    #[test]
    fn download_default_name() {
        assert_eq!(
            default_local_download_path("/var/log/syslog", None),
            PathBuf::from("syslog")
        );
    }
}
