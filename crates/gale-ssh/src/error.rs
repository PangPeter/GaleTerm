use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("ssh error: {0}")]
    Ssh(String),

    #[error("auth failed")]
    AuthFailed,

    #[error("cancelled by user")]
    Cancelled,

    #[error("missing ssh config")]
    MissingConfig,

    #[error(
        "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\n\
         Host key for {host} does not match known_hosts (line {line}).\n\
         Offered fingerprint: {fingerprint}\n\
         known_hosts file: {known_hosts}\n\
         Someone could be eavesdropping on you (man-in-the-middle attack).\n\
         If you expected this change, remove the old key and try again."
    )]
    HostKeyChanged {
        host: String,
        line: usize,
        fingerprint: String,
        known_hosts: String,
    },

    #[error("host key verification failed for {host} ({fingerprint})")]
    HostKeyRejected { host: String, fingerprint: String },

    #[error("ssh agent: {0}")]
    Agent(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
