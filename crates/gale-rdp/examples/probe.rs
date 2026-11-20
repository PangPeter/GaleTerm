//! Connect to an RDP server, wait for the first graphics update, exit.
//!
//! ```shell
//! cargo run -p gale-rdp --example probe -- 127.0.0.1 3389 user password
//! ```

use std::env;
use std::sync::Arc;
use std::time::Duration;

use gale_core::vault::CredentialKey;
use gale_rdp::{
    RdpCertDecision, RdpCertInfo, RdpClientConfig, RdpInput, RdpOutput, RdpPrompt, RdpSession,
};
use tokio::sync::mpsc;

struct EnvPrompt;

impl RdpPrompt for EnvPrompt {
    fn ask_password(&self, _: &str) -> Option<String> {
        None
    }

    fn saved_credential(&self, _: &CredentialKey) -> Option<String> {
        None
    }

    fn confirm_certificate(&self, info: &RdpCertInfo) -> RdpCertDecision {
        eprintln!(
            "accepting untrusted cert for {}:{} ({})",
            info.host, info.port, info.fingerprint
        );
        RdpCertDecision::AcceptOnce
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();

    let mut args = env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args
        .next()
        .unwrap_or_else(|| "3389".into())
        .parse()
        .unwrap_or(3389);
    let username = args.next().unwrap_or_else(|| "ubuntu".into());
    let password = args.next().unwrap_or_else(|| "ubuntu".into());

    let config = RdpClientConfig {
        host: host.clone(),
        port,
        username: username.clone(),
        domain: None,
        password: Some(password),
        prompt: Some(Arc::new(EnvPrompt)),
        width: 1024,
        height: 768,
        dial: None,
    };

    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();

    let join = tokio::task::spawn_blocking(move || {
        let session = RdpSession::connect(config)?;
        session.run(input_rx, output_tx)?;
        Ok::<(), anyhow::Error>(())
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut got_frame = false;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, output_rx.recv()).await {
            Ok(Some(RdpOutput::Ready { width, height, name })) => {
                println!("ready {width}x{height} ({name})");
            }
            Ok(Some(RdpOutput::Frame(frame))) => {
                let non_black = frame
                    .pixels
                    .chunks_exact(4)
                    .filter(|p| p[0] | p[1] | p[2] != 0)
                    .count();
                println!(
                    "frame {}x{} non_black_pixels={non_black}",
                    frame.width, frame.height
                );
                got_frame = true;
                // Hold so post-logon channels (RDPDR announce) can surface PDU errors.
                let hold: u64 = env::var("HOLD_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if hold == 0 {
                    break;
                }
                println!("holding {hold}s for channel PDUs…");
            }
            Ok(Some(RdpOutput::CutText(t))) if t.starts_with("error:") => {
                anyhow::bail!("{}", t.trim_start_matches("error:"));
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let _ = input_tx.send(RdpInput::Close);
    match join.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(e.into()),
    }

    if !got_frame {
        anyhow::bail!("no graphics update from {username}@{host}:{port} within 30s");
    }
    Ok(())
}
