//! Open a local SSH tunnel and print the ephemeral port.
//!
//! ```shell
//! cargo run -p gale-ssh --example tunnel_probe -- \
//!   --ssh-host 127.0.0.1 --ssh-port 2222 --ssh-user ubuntu --ssh-password ubuntu \
//!   --dest 127.0.0.1:3389
//! ```

use std::env;
use std::sync::Arc;
use std::time::Duration;

use gale_core::{AuthMethod, SshAuth, SshConfig};
use gale_ssh::{open_local_tunnel, SecretPrompt, SshSessionConfig};

struct FixedPassword(String);

impl SecretPrompt for FixedPassword {
    fn ask_secret(&self, _: &str) -> Option<String> {
        Some(self.0.clone())
    }
}

fn arg(flag: &str) -> Option<String> {
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ssh_host = arg("--ssh-host").unwrap_or_else(|| "127.0.0.1".into());
    let ssh_port: u16 = arg("--ssh-port")
        .unwrap_or_else(|| "22".into())
        .parse()?;
    let ssh_user = arg("--ssh-user").unwrap_or_else(|| "ubuntu".into());
    let ssh_password = arg("--ssh-password").unwrap_or_else(|| "ubuntu".into());
    let dest = arg("--dest").unwrap_or_else(|| "127.0.0.1:3389".into());
    let (dest_host, dest_port) = gale_ssh::parse_host_port(&dest)?;

    let mut cfg = SshSessionConfig::from_ssh_config(
        &SshConfig {
            host: ssh_host,
            port: ssh_port,
            username: ssh_user,
            auth: SshAuth {
                method: AuthMethod::Password,
                password: Some(ssh_password.clone()),
                private_key_path: None,
                passphrase: None,
            },
            jump_session_id: None,
            proxy: None,
        },
        80,
        24,
    );
    cfg.host_key_policy = gale_ssh::HostKeyPolicy::AcceptNew;
    cfg.secret_prompt = Some(Arc::new(FixedPassword(ssh_password)));

    let tunnel = open_local_tunnel(cfg, dest_host, dest_port).await?;
    println!("tunnel ready on {}", tunnel.local_addr());

    // Keep alive briefly so a follow-up client can dial.
    let hold = arg("--hold-secs")
        .unwrap_or_else(|| "2".into())
        .parse::<u64>()
        .unwrap_or(2);
    tokio::time::sleep(Duration::from_secs(hold)).await;
    Ok(())
}
