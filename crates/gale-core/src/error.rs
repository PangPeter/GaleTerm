use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("invalid session: {0}")]
    InvalidSession(String),

    #[error("proxy jump cycle detected involving session `{0}`")]
    JumpCycle(String),

    #[error("proxy jump chain too deep (max {0})")]
    JumpTooDeep(usize),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}
