use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize as RawPtySize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
        }
    }
}

impl From<PtySize> for RawPtySize {
    fn from(value: PtySize) -> Self {
        RawPtySize {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// Local interactive shell over a platform PTY.
pub struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl LocalPty {
    /// Interactive login shell in the user's home directory.
    pub fn spawn_shell(size: PtySize) -> Result<Self> {
        let shell = default_shell();
        let args: &[&str] = if cfg!(windows) { &[] } else { &["-l"] };
        let cwd = home_dir().or_else(|| std::env::current_dir().ok());
        Self::spawn_in(&shell, args, cwd, size)
    }

    pub fn spawn_command(program: &str, args: &[&str], size: PtySize) -> Result<Self> {
        Self::spawn_in(program, args, std::env::current_dir().ok(), size)
    }

    fn spawn_in(
        program: &str,
        args: &[&str],
        cwd: Option<std::path::PathBuf>,
        size: PtySize,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size.into())
            .map_err(|e| Error::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        // The host may run with TERM=dumb (IDE / launchd); the emulator is xterm-compatible.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "GaleTerm");
        cmd.env_remove("TERM_PROGRAM_VERSION");
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Pty(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::Pty(e.to_string()))?;

        Ok(Self {
            master: pair.master,
            child,
            writer: Arc::new(Mutex::new(writer)),
        })
    }

    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.master
            .resize(size.into())
            .map_err(|e| Error::Pty(e.to_string()))
    }

    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| Error::Pty("pty writer lock poisoned".into()))?;
        guard.write_all(data)?;
        guard.flush()?;
        Ok(())
    }

    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master
            .try_clone_reader()
            .map_err(|e| Error::Pty(e.to_string()))
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

fn home_dir() -> Option<std::path::PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(std::path::PathBuf::from)
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}
