//! Cross-platform serial port session (tokio-serial).

use gale_core::{SerialConfig, SerialParity, SessionInput, SessionProfile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, StopBits};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct SerialPortInfo {
    pub name: String,
    pub port_type: String,
}

#[derive(Debug, Clone)]
pub struct SerialSessionConfig {
    pub device: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: SerialParity,
}

impl SerialSessionConfig {
    pub fn new(device: impl Into<String>, baud_rate: u32) -> Self {
        Self {
            device: device.into(),
            baud_rate,
            data_bits: 8,
            stop_bits: 1,
            parity: SerialParity::None,
        }
    }

    pub fn from_profile(profile: &SessionProfile) -> Result<Self> {
        let s = profile
            .serial
            .as_ref()
            .ok_or_else(|| Error::Serial("session missing serial config".into()))?;
        Ok(Self::from_serial_config(s))
    }

    pub fn from_serial_config(s: &SerialConfig) -> Self {
        Self {
            device: s.device.clone(),
            baud_rate: s.baud_rate,
            data_bits: s.data_bits,
            stop_bits: s.stop_bits,
            parity: s.parity,
        }
    }
}

pub struct SerialSession {
    port: tokio_serial::SerialStream,
}

impl SerialSession {
    pub async fn open(config: SerialSessionConfig) -> Result<Self> {
        let data_bits = match config.data_bits {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            8 => DataBits::Eight,
            other => {
                return Err(Error::Serial(format!(
                    "unsupported data bits: {other} (use 5–8)"
                )))
            }
        };
        let stop_bits = match config.stop_bits {
            1 => StopBits::One,
            2 => StopBits::Two,
            other => {
                return Err(Error::Serial(format!(
                    "unsupported stop bits: {other} (use 1 or 2)"
                )))
            }
        };
        let parity = match config.parity {
            SerialParity::None => Parity::None,
            SerialParity::Even => Parity::Even,
            SerialParity::Odd => Parity::Odd,
        };

        let port = tokio_serial::new(&config.device, config.baud_rate)
            .data_bits(data_bits)
            .stop_bits(stop_bits)
            .parity(parity)
            .flow_control(FlowControl::None)
            .open_native_async()
            .map_err(|e| {
                Error::Serial(format!("failed to open {}: {e}", config.device))
            })?;

        Ok(Self { port })
    }

    /// Drive the port from UI channels until it closes or the UI sends
    /// [`SessionInput::Close`] / drops `input`. Resize is ignored.
    pub async fn run_channels(
        mut self,
        mut input: mpsc::UnboundedReceiver<SessionInput>,
        output: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        let mut port_buf = [0u8; 8192];

        loop {
            tokio::select! {
                cmd = input.recv() => match cmd {
                    Some(SessionInput::Data(data)) => {
                        self.port.write_all(&data).await?;
                        self.port.flush().await?;
                    }
                    Some(SessionInput::Resize { .. }) => {}
                    Some(SessionInput::Close) | None => break,
                },
                read = self.port.read(&mut port_buf) => {
                    let n = read?;
                    if n == 0 || output.send(port_buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn relay_stdio(mut self) -> Result<()> {
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut stdin_buf = [0u8; 4096];
        let mut port_buf = [0u8; 8192];

        loop {
            tokio::select! {
                read = stdin.read(&mut stdin_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    self.port.write_all(&stdin_buf[..n]).await?;
                    self.port.flush().await?;
                }
                read = self.port.read(&mut port_buf) => {
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    stdout.write_all(&port_buf[..n]).await?;
                    stdout.flush().await?;
                }
            }
        }
        Ok(())
    }
}

/// List available serial ports on this machine.
pub fn list_ports() -> Result<Vec<SerialPortInfo>> {
    let ports = tokio_serial::available_ports().map_err(|e| {
        Error::Serial(format!("list ports: {e}"))
    })?;
    Ok(ports
        .into_iter()
        .map(|p| SerialPortInfo {
            name: p.port_name,
            port_type: format!("{:?}", p.port_type),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_new_serial_profile() {
        let profile = SessionProfile::new_serial("uart", "/dev/tty.usbserial", 115_200);
        let cfg = SerialSessionConfig::from_profile(&profile).unwrap();
        assert_eq!(cfg.device, "/dev/tty.usbserial");
        assert_eq!(cfg.baud_rate, 115_200);
        assert_eq!(cfg.data_bits, 8);
    }
}
