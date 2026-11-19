//! Raw TCP session (no Telnet IAC).

use gale_core::SessionInput;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct TcpSessionConfig {
    pub host: String,
    pub port: u16,
}

impl TcpSessionConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    pub fn from_profile(profile: &gale_core::SessionProfile) -> Result<Self> {
        let tcp = profile
            .tcp
            .as_ref()
            .ok_or_else(|| Error::Net("session missing tcp config".into()))?;
        Ok(Self {
            host: tcp.host.clone(),
            port: tcp.port,
        })
    }
}

pub struct TcpSession {
    stream: TcpStream,
}

impl TcpSession {
    pub async fn connect(config: TcpSessionConfig) -> Result<Self> {
        let stream = TcpStream::connect((config.host.as_str(), config.port))
            .await
            .map_err(|e| {
                Error::Net(format!(
                    "failed to connect to {}:{}: {e}",
                    config.host, config.port
                ))
            })?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream })
    }

    /// Drive the session from UI channels until the peer closes or the UI
    /// sends [`SessionInput::Close`] / drops `input`. Resize is ignored.
    pub async fn run_channels(
        mut self,
        mut input: mpsc::UnboundedReceiver<SessionInput>,
        output: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        let (mut reader, mut writer) = self.stream.split();
        let mut net_buf = [0u8; 8192];

        loop {
            tokio::select! {
                cmd = input.recv() => match cmd {
                    Some(SessionInput::Data(data)) => writer.write_all(&data).await?,
                    Some(SessionInput::Resize { .. }) => {}
                    Some(SessionInput::Close) | None => break,
                },
                read = reader.read(&mut net_buf) => {
                    let n = read?;
                    if n == 0 || output.send(net_buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.shutdown().await;
        Ok(())
    }

    pub async fn relay_stdio(mut self) -> Result<()> {
        let (mut reader, mut writer) = self.stream.split();
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut stdin_buf = [0u8; 4096];
        let mut net_buf = [0u8; 8192];

        loop {
            tokio::select! {
                read = stdin.read(&mut stdin_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&stdin_buf[..n]).await?;
                }
                read = reader.read(&mut net_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    stdout.write_all(&net_buf[..n]).await?;
                    stdout.flush().await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn run_channels_round_trips_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&buf[..n]).await.unwrap();
            // Peer closes after the UI sends Close.
            assert_eq!(sock.read(&mut buf).await.unwrap(), 0);
        });

        let session = TcpSession::connect(TcpSessionConfig::new("127.0.0.1", port))
            .await
            .unwrap();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(session.run_channels(in_rx, out_tx));

        in_tx.send(SessionInput::Data(b"ping".to_vec())).unwrap();
        assert_eq!(out_rx.recv().await.unwrap(), b"ping");
        in_tx.send(SessionInput::Close).unwrap();

        run.await.unwrap().unwrap();
        server.await.unwrap();
    }
}
