//! Telnet and raw TCP clients for GaleTerm.

mod error;
mod telnet;
mod tcp;

pub use error::{Error, Result};
pub use tcp::{TcpSession, TcpSessionConfig};
pub use telnet::{TelnetSession, TelnetSessionConfig};
