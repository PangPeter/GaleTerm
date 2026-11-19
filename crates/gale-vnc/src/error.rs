use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("unsupported security type")]
    UnsupportedSecurity,

    #[error("authentication failed")]
    AuthFailed,

    #[error("authentication cancelled")]
    Cancelled,

    #[error("connection closed")]
    Closed,

    #[error("missing vnc config")]
    MissingConfig,

    #[error("{0}")]
    Other(String),
}
