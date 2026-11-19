//! File-system backends for the dual-pane browser. Every protocol (local disk,
//! SFTP, later FTP/FTPS) exposes the same listing / mutation / transfer surface
//! so the panes never care what is behind them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use gale_ftp::FtpClient;
use gale_ssh::{SftpClient, TransferProgress};

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: Option<u64>,
    /// Seconds since the Unix epoch.
    pub modified: Option<i64>,
    pub permissions: Option<u32>,
}

impl FileEntry {
    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }
}

#[derive(Clone)]
pub enum Backend {
    Local,
    Sftp(Arc<SftpClient>),
    Ftp(Arc<FtpClient>),
}

impl Backend {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// Plain FTP sends credentials and data in clear text.
    pub fn is_insecure(&self) -> bool {
        matches!(self, Self::Ftp(ftp) if !ftp.is_secure())
    }

    pub async fn home(&self) -> anyhow::Result<String> {
        match self {
            Self::Local => Ok(dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .to_string_lossy()
                .into_owned()),
            Self::Sftp(sftp) => Ok(sftp.pwd().await?),
            Self::Ftp(ftp) => Ok(ftp.pwd().await?),
        }
    }

    /// Directory contents without `.` / `..`, folders first, then by name.
    pub async fn list(&self, path: &str) -> anyhow::Result<Vec<FileEntry>> {
        let mut entries = match self {
            Self::Local => list_local(Path::new(path)).await?,
            Self::Sftp(sftp) => sftp
                .list(path)
                .await?
                .into_iter()
                .filter(|e| e.name != "." && e.name != "..")
                .map(|e| FileEntry {
                    name: e.name,
                    is_dir: e.is_dir,
                    is_symlink: e.is_symlink,
                    size: e.size,
                    modified: e.modified.map(i64::from),
                    permissions: e.permissions,
                })
                .collect(),
            Self::Ftp(ftp) => ftp
                .list(path)
                .await?
                .into_iter()
                .map(|e| FileEntry {
                    name: e.name,
                    is_dir: e.is_dir,
                    is_symlink: e.is_symlink,
                    size: e.size,
                    modified: e.modified,
                    permissions: e.permissions,
                })
                .collect(),
        };
        sort_entries(&mut entries);
        Ok(entries)
    }

    pub async fn mkdir(&self, path: &str) -> anyhow::Result<()> {
        match self {
            Self::Local => tokio::fs::create_dir(path)
                .await
                .with_context(|| format!("mkdir `{path}`"))?,
            Self::Sftp(sftp) => sftp.mkdir(path).await?,
            Self::Ftp(ftp) => ftp.mkdir(path).await?,
        }
        Ok(())
    }

    pub async fn remove(&self, path: &str, is_dir: bool) -> anyhow::Result<()> {
        match self {
            Self::Local if is_dir => tokio::fs::remove_dir_all(path)
                .await
                .with_context(|| format!("rm -r `{path}`"))?,
            Self::Local => tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("rm `{path}`"))?,
            Self::Sftp(sftp) => sftp.rm(path, is_dir).await?,
            Self::Ftp(ftp) => ftp.remove(path, is_dir).await?,
        }
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> anyhow::Result<()> {
        match self {
            Self::Local => tokio::fs::rename(from, to)
                .await
                .with_context(|| format!("mv `{from}` -> `{to}`"))?,
            Self::Sftp(sftp) => sftp.rename(from, to).await?,
            Self::Ftp(ftp) => ftp.rename(from, to).await?,
        }
        Ok(())
    }

    /// Create an empty file; fails if something with that name already exists.
    pub async fn create_file(&self, path: &str) -> anyhow::Result<()> {
        match self {
            Self::Local => {
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .await
                    .with_context(|| format!("touch `{path}`"))?;
            }
            Self::Sftp(sftp) => sftp.create_file(path).await?,
            Self::Ftp(ftp) => ftp.create_file(path).await?,
        }
        Ok(())
    }

    pub async fn chmod(&self, path: &str, mode: u32) -> anyhow::Result<()> {
        match self {
            Self::Local => set_local_mode(path, mode).await?,
            Self::Sftp(sftp) => sftp.chmod(path, mode).await?,
            Self::Ftp(ftp) => ftp.chmod(path, mode).await?,
        }
        Ok(())
    }

    pub fn join(&self, dir: &str, name: &str) -> String {
        match self {
            Self::Local => Path::new(dir).join(name).to_string_lossy().into_owned(),
            Self::Sftp(_) | Self::Ftp(_) => gale_ssh::join_remote(dir, name),
        }
    }

    pub fn parent(&self, path: &str) -> Option<String> {
        match self {
            Self::Local => Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned()),
            Self::Sftp(_) | Self::Ftp(_) => remote_parent(path),
        }
    }
}

/// Copy `src` (a file or folder named `name`) from one backend into the
/// directory `dst_dir` on another.
pub async fn transfer(
    from: &Backend,
    src: &str,
    to: &Backend,
    dst_dir: &str,
    name: &str,
    is_dir: bool,
    progress: TransferProgress<'_>,
) -> anyhow::Result<u64> {
    let dst = to.join(dst_dir, name);
    let bytes = match (from, to) {
        (Backend::Local, Backend::Sftp(sftp)) if is_dir => {
            sftp.upload_dir_with_progress(Path::new(src), &dst, progress).await?
        }
        (Backend::Local, Backend::Sftp(sftp)) => {
            sftp.upload_with_progress(Path::new(src), &dst, progress).await?
        }
        (Backend::Sftp(sftp), Backend::Local) if is_dir => {
            sftp.download_dir_with_progress(src, Path::new(&dst), progress).await?
        }
        (Backend::Sftp(sftp), Backend::Local) => {
            sftp.download_with_progress(src, Path::new(&dst), progress).await?
        }
        // FTP has one transfer per connection; use a second one so the pane
        // stays browsable while files move.
        (Backend::Local, Backend::Ftp(ftp)) => {
            let conn = ftp.fork().await?;
            if is_dir {
                conn.upload_dir_with_progress(Path::new(src), &dst, progress).await?
            } else {
                conn.upload_with_progress(Path::new(src), &dst, progress).await?
            }
        }
        (Backend::Ftp(ftp), Backend::Local) => {
            let conn = ftp.fork().await?;
            if is_dir {
                conn.download_dir_with_progress(src, Path::new(&dst), progress).await?
            } else {
                conn.download_with_progress(src, Path::new(&dst), progress).await?
            }
        }
        _ => bail!("transfers between these locations are not supported yet"),
    };
    Ok(bytes)
}

async fn list_local(path: &Path) -> anyhow::Result<Vec<FileEntry>> {
    let mut rd = tokio::fs::read_dir(path)
        .await
        .with_context(|| format!("ls `{}`", path.display()))?;
    let mut entries = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let link = entry.metadata().await.ok();
        let is_symlink = entry.file_type().await.is_ok_and(|t| t.is_symlink());
        // Follow links so a link to a folder can be opened like one.
        let meta = if is_symlink {
            tokio::fs::metadata(entry.path()).await.ok().or(link)
        } else {
            link
        };
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            is_symlink,
            size: meta.as_ref().filter(|m| !m.is_dir()).map(|m| m.len()),
            modified: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            permissions: meta.as_ref().and_then(local_mode),
        });
    }
    Ok(entries)
}

#[cfg(unix)]
fn local_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn local_mode(_: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
async fn set_local_mode(path: &str, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
        .await
        .with_context(|| format!("chmod `{path}`"))
}

#[cfg(not(unix))]
async fn set_local_mode(_: &str, _: u32) -> anyhow::Result<()> {
    bail!("changing permissions is not supported on this platform")
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn remote_parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".into()),
        Some(ix) => Some(trimmed[..ix].to_string()),
        None => Some(".".into()),
    }
}

/// `drwxr-xr-x` style mode string.
pub fn format_mode(perms: u32, is_dir: bool, is_symlink: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_symlink {
        'l'
    } else if is_dir {
        'd'
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

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{format_mode, format_size, remote_parent};

    #[test]
    fn remote_parent_walks_up_to_root() {
        assert_eq!(remote_parent("/home/makerv").as_deref(), Some("/home"));
        assert_eq!(remote_parent("/home/").as_deref(), Some("/"));
        assert_eq!(remote_parent("/home").as_deref(), Some("/"));
        assert_eq!(remote_parent("/"), None);
        assert_eq!(remote_parent("docs").as_deref(), Some("."));
    }

    #[test]
    fn sizes_and_modes_are_human_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_mode(0o755, true, false), "drwxr-xr-x");
        assert_eq!(format_mode(0o777, true, true), "lrwxrwxrwx");
    }
}
