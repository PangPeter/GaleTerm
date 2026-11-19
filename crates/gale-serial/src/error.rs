use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("serial error: {0}")]
    Serial(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
