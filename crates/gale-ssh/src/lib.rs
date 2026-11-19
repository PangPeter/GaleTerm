//! SSH client for GaleTerm (auth + known_hosts/TOFU + ProxyJump + port forward + SFTP/SCP).

mod agent;
mod client;
mod error;
mod forward;
mod host_key;
mod proxy;
mod scp;
mod sftp;

pub use agent::connect_agent;
pub use client::{SecretPrompt, SshSession, SshSessionConfig};
pub use error::{Error, Result};
pub use forward::{
    open_local_tunnel, parse_bind_addr, parse_host_port, run_forward, ForwardMode, ForwardSpec,
    LocalTunnel,
};
pub use gale_core::{ProxyConfig, ProxyKind};
pub use host_key::{
    default_known_hosts_path, verify_host_key, AcceptAndLearnUnknown, CliHostKeyPrompt,
    HostKeyDecision, HostKeyInfo, HostKeyPolicy, HostKeyPrompt, RejectUnknown,
};
pub use proxy::{dial_via_proxy, parse_proxy_url};
pub use scp::{default_local_scp_path, ScpClient};
pub use sftp::{
    default_local_download_path, join_remote, SftpClient, SftpListEntry, TransferProgress,
};
