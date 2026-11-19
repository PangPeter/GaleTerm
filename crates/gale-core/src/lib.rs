//! GaleTerm core: session models and persistence.

mod error;
mod io;
mod session;
mod store;
pub mod vault;

pub use error::{Error, Result};
pub use io::SessionInput;
pub use session::{
    AuthMethod, FtpConfig, FtpSecurity, Protocol, ProxyConfig, ProxyKind, RdpConfig, SerialConfig,
    SerialParity, SessionId, SessionProfile, SshAuth, SshConfig, TcpConfig, TelnetConfig, VncConfig,
};
pub use store::{SessionStore, MAX_JUMP_DEPTH};
