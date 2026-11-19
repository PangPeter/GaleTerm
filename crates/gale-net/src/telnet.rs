//! Minimal Telnet client with IAC option negotiation.

use gale_core::SessionInput;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

const OPT_BINARY: u8 = 0;
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3; // Suppress Go Ahead
const OPT_TTYPE: u8 = 24; // Terminal Type
const OPT_NAWS: u8 = 31; // Negotiate About Window Size

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

#[derive(Debug, Clone)]
pub struct TelnetSessionConfig {
    pub host: String,
    pub port: u16,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
}

impl TelnetSessionConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            cols: 80,
            rows: 24,
            term: "xterm-256color".into(),
        }
    }

    pub fn from_profile(profile: &gale_core::SessionProfile, cols: u16, rows: u16) -> Result<Self> {
        let telnet = profile
            .telnet
            .as_ref()
            .ok_or_else(|| Error::Net("session missing telnet config".into()))?;
        Ok(Self {
            host: telnet.host.clone(),
            port: telnet.port,
            cols,
            rows,
            term: "xterm-256color".into(),
        })
    }
}

pub struct TelnetSession {
    stream: TcpStream,
    cols: u16,
    rows: u16,
    term: String,
}

impl TelnetSession {
    pub async fn connect(config: TelnetSessionConfig) -> Result<Self> {
        let stream = TcpStream::connect((config.host.as_str(), config.port))
            .await
            .map_err(|e| {
                Error::Net(format!(
                    "failed to connect to {}:{}: {e}",
                    config.host, config.port
                ))
            })?;
        stream.set_nodelay(true).ok();
        let mut session = Self {
            stream,
            cols: config.cols,
            rows: config.rows,
            term: config.term,
        };
        // Offer common client capabilities up front.
        session
            .write_raw(&[IAC, WILL, OPT_TTYPE, IAC, WILL, OPT_NAWS, IAC, DO, OPT_SGA])
            .await?;
        session.send_naws().await?;
        Ok(session)
    }

    async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data).await?;
        Ok(())
    }

    async fn send_naws(&mut self) -> Result<()> {
        let cols = self.cols.to_be_bytes();
        let rows = self.rows.to_be_bytes();
        self.write_raw(&[
            IAC, SB, OPT_NAWS, cols[0], cols[1], rows[0], rows[1], IAC, SE,
        ])
        .await
    }

    async fn send_ttype(&mut self) -> Result<()> {
        let mut msg = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
        msg.extend_from_slice(self.term.as_bytes());
        msg.extend_from_slice(&[IAC, SE]);
        self.write_raw(&msg).await
    }

    fn decide_do(&self, opt: u8) -> u8 {
        match opt {
            OPT_SGA | OPT_BINARY | OPT_ECHO => WILL,
            OPT_TTYPE | OPT_NAWS => WILL,
            _ => WONT,
        }
    }

    fn decide_will(&self, opt: u8) -> u8 {
        match opt {
            OPT_SGA | OPT_BINARY | OPT_ECHO => DO,
            _ => DONT,
        }
    }

    /// Strip IAC sequences from `input`, write replies, append payload to `out`.
    async fn process_iac(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let mut i = 0;
        while i < input.len() {
            if input[i] != IAC {
                out.push(input[i]);
                i += 1;
                continue;
            }
            i += 1;
            if i >= input.len() {
                break;
            }
            match input[i] {
                IAC => {
                    out.push(IAC);
                    i += 1;
                }
                WILL => {
                    i += 1;
                    if i >= input.len() {
                        break;
                    }
                    let opt = input[i];
                    i += 1;
                    let reply = self.decide_will(opt);
                    self.write_raw(&[IAC, reply, opt]).await?;
                }
                WONT => {
                    i += 1;
                    if i >= input.len() {
                        break;
                    }
                    let opt = input[i];
                    i += 1;
                    self.write_raw(&[IAC, DONT, opt]).await?;
                }
                DO => {
                    i += 1;
                    if i >= input.len() {
                        break;
                    }
                    let opt = input[i];
                    i += 1;
                    let reply = self.decide_do(opt);
                    self.write_raw(&[IAC, reply, opt]).await?;
                    if opt == OPT_NAWS && reply == WILL {
                        self.send_naws().await?;
                    }
                }
                DONT => {
                    i += 1;
                    if i >= input.len() {
                        break;
                    }
                    let opt = input[i];
                    i += 1;
                    self.write_raw(&[IAC, WONT, opt]).await?;
                }
                SB => {
                    i += 1;
                    let start = i;
                    // Find IAC SE
                    while i + 1 < input.len() {
                        if input[i] == IAC && input[i + 1] == SE {
                            break;
                        }
                        i += 1;
                    }
                    let sub = &input[start..i];
                    if i + 1 < input.len() {
                        i += 2; // skip IAC SE
                    }
                    if sub.len() >= 2 && sub[0] == OPT_TTYPE && sub[1] == TTYPE_SEND {
                        self.send_ttype().await?;
                    }
                }
                _ => {
                    // Ignore other commands (NOP, GA, etc.)
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// Escape outbound data so literal 0xFF becomes IAC IAC.
    fn escape_iac(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            out.push(b);
            if b == IAC {
                out.push(IAC);
            }
        }
        out
    }

    /// Drive the session from UI channels until the peer closes or the UI
    /// sends [`SessionInput::Close`] / drops `input`.
    pub async fn run_channels(
        mut self,
        mut input: mpsc::UnboundedReceiver<SessionInput>,
        output: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        let mut net_buf = [0u8; 8192];
        let mut payload = Vec::with_capacity(8192);
        let mut pending = Vec::new();

        loop {
            tokio::select! {
                cmd = input.recv() => match cmd {
                    Some(SessionInput::Data(data)) => {
                        let escaped = Self::escape_iac(&data);
                        self.stream.write_all(&escaped).await?;
                    }
                    Some(SessionInput::Resize { cols, rows }) => {
                        self.cols = cols;
                        self.rows = rows;
                        self.send_naws().await?;
                    }
                    Some(SessionInput::Close) | None => break,
                },
                read = self.stream.read(&mut net_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    pending.extend_from_slice(&net_buf[..n]);
                    let keep = trailing_partial_iac(&pending);
                    let process_len = pending.len() - keep;
                    if process_len > 0 {
                        let chunk: Vec<u8> = pending.drain(..process_len).collect();
                        payload.clear();
                        self.process_iac(&chunk, &mut payload).await?;
                        if !payload.is_empty() && output.send(payload.clone()).is_err() {
                            break;
                        }
                    }
                }
            }
        }
        let _ = self.stream.shutdown().await;
        Ok(())
    }

    pub async fn relay_stdio(mut self) -> Result<()> {
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut stdin_buf = [0u8; 4096];
        let mut net_buf = [0u8; 8192];
        let mut payload = Vec::with_capacity(8192);
        // Incomplete IAC at buffer boundary — keep leftover.
        let mut pending = Vec::new();

        loop {
            tokio::select! {
                read = stdin.read(&mut stdin_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    let escaped = Self::escape_iac(&stdin_buf[..n]);
                    self.stream.write_all(&escaped).await?;
                }
                read = self.stream.read(&mut net_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    pending.extend_from_slice(&net_buf[..n]);
                    // Keep a few trailing bytes if they look like a partial IAC seq.
                    let keep = trailing_partial_iac(&pending);
                    let process_len = pending.len() - keep;
                    if process_len > 0 {
                        let chunk: Vec<u8> = pending.drain(..process_len).collect();
                        payload.clear();
                        self.process_iac(&chunk, &mut payload).await?;
                        if !payload.is_empty() {
                            stdout.write_all(&payload).await?;
                            stdout.flush().await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn trailing_partial_iac(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let last = buf[buf.len() - 1];
    if last == IAC {
        return 1;
    }
    if buf.len() >= 2 && buf[buf.len() - 2] == IAC {
        match last {
            WILL | WONT | DO | DONT | SB => return 2,
            _ => {}
        }
    }
    // Inside SB ... incomplete: keep from last IAC SB
    if let Some(pos) = buf.iter().rposition(|&b| b == SB) {
        if pos > 0 && buf[pos - 1] == IAC {
            let after = &buf[pos + 1..];
            if !after.windows(2).any(|w| w == [IAC, SE]) {
                return buf.len() - (pos - 1);
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_doubles_iac() {
        assert_eq!(TelnetSession::escape_iac(&[1, 255, 2]), vec![1, 255, 255, 2]);
    }

    #[test]
    fn trailing_keeps_partial_will() {
        assert_eq!(trailing_partial_iac(&[IAC, WILL]), 2);
        assert_eq!(trailing_partial_iac(&[IAC]), 1);
        assert_eq!(trailing_partial_iac(&[b'a', b'b']), 0);
    }
}
