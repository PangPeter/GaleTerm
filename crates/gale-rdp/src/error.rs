use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("session is missing an RDP config")]
    MissingConfig,
    #[error("RDP authentication failed")]
    AuthFailed,
    #[error("password required")]
    PasswordRequired,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
