//! GaleTerm core: session models and persistence.

mod error;
mod io;
mod session;
mod store;

pub use error::{Error, Result};
pub use io::SessionInput;
pub use session::{
    AuthMethod, Protocol, ProxyConfig, ProxyKind, SerialConfig, SerialParity, FtpConfig, FtpSecurity, SessionId,
    SessionProfile, SshAuth, SshConfig, TcpConfig, TelnetConfig,
};
pub use store::{SessionStore, MAX_JUMP_DEPTH};
