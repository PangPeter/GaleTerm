use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("server replied {code}: {message}")]
    Reply { code: u16, message: String },

    #[error("ftp protocol error: {0}")]
    Protocol(String),

    #[error("tls: {0}")]
    Tls(String),

    #[error("server does not support FTPS (AUTH TLS was refused)")]
    TlsUnavailable,

    #[error("login failed: {0}")]
    Login(String),

    #[error("connection timed out")]
    Timeout,

    #[error("connection closed by server")]
    Closed,

    #[error("cancelled")]
    Cancelled,

    #[error("missing ftp config")]
    MissingConfig,
}

impl Error {
    /// The control connection is unusable and should be re-established.
    pub fn is_connection_lost(&self) -> bool {
        match self {
            Self::Io(_) | Self::Closed | Self::Timeout => true,
            Self::Reply { code, .. } => *code == 421,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
