use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Net(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
