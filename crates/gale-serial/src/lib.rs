//! Serial port sessions for GaleTerm.

mod error;
mod session;

pub use error::{Error, Result};
pub use session::{list_ports, SerialPortInfo, SerialSession, SerialSessionConfig};
