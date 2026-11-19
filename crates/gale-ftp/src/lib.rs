//! FTP / FTPS client for GaleTerm: passive mode, MLSD / LIST parsing,
//! explicit (`AUTH TLS`) and implicit TLS with TOFU certificate prompts.

mod client;
mod error;
mod list;
mod tls;

pub use client::{join_path, FtpClient, FtpClientConfig, TransferProgress};
pub use error::{Error, Result};
pub use list::FtpEntry;
pub use tls::{default_known_certs_path, CertDecision, CertInfo, FtpPrompt};
