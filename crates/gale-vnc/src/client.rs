//! RFB 3.8 client: connect, authenticate, maintain a local framebuffer.

use std::sync::Arc;

use gale_core::vault::CredentialKey;
use gale_core::{SessionProfile, VncConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::auth::encrypt_challenge;
use crate::error::{Error, Result};
use crate::pixel::{decode_pixels, PixelFormat};

const RFB_VERSION: &[u8] = b"RFB 003.008\n";
const SEC_INVALID: u8 = 0;
const SEC_NONE: u8 = 1;
const SEC_VNC: u8 = 2;
const ENC_RAW: i32 = 0;
const ENC_COPY_RECT: i32 = 1;

/// Asks for the VNC password when none is configured.
pub trait VncPrompt: Send + Sync {
    fn ask_password(&self, prompt: &str) -> Option<String>;

    fn ask_credential(&self, _key: &CredentialKey, prompt: &str) -> Option<String> {
        self.ask_password(prompt)
    }

    fn saved_credential(&self, _key: &CredentialKey) -> Option<String> {
        None
    }

    fn credential_result(&self, _key: &CredentialKey, _secret: &str, _accepted: bool) {}
}

#[derive(Clone)]
pub struct VncClientConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub prompt: Option<Arc<dyn VncPrompt>>,
    /// Shared desktop (true) vs exclusive (false).
    pub shared: bool,
    /// Dial this endpoint instead of `host:port` (SSH local tunnel).
    pub dial: Option<(String, u16)>,
}

impl VncClientConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            password: None,
            prompt: None,
            shared: true,
            dial: None,
        }
    }

    pub fn from_profile(profile: &SessionProfile) -> Result<Self> {
        let vnc = profile.vnc.as_ref().ok_or(Error::MissingConfig)?;
        Ok(Self::from_vnc(vnc))
    }

    pub fn from_vnc(vnc: &VncConfig) -> Self {
        Self {
            host: vnc.host.clone(),
            port: vnc.port,
            password: vnc.password.clone(),
            prompt: None,
            shared: true,
            dial: None,
        }
    }

    pub fn with_prompt(mut self, prompt: Arc<dyn VncPrompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }

    pub fn with_dial(mut self, host: impl Into<String>, port: u16) -> Self {
        self.dial = Some((host.into(), port));
        self
    }

    fn dial_addr(&self) -> (String, u16) {
        self.dial
            .clone()
            .unwrap_or_else(|| (self.host.clone(), self.port))
    }
}

/// Full desktop pixels in BGRA8 (gpui atlas layout).
#[derive(Clone)]
pub struct FrameBuffer {
    pub width: u16,
    pub height: u16,
    pub pixels: Arc<[u8]>,
}

impl FrameBuffer {
    fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize * 4;
        Self {
            width,
            height,
            pixels: vec![0u8; len].into(),
        }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        Arc::make_mut(&mut self.pixels)
    }
}

#[derive(Debug, Clone)]
pub enum VncInput {
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { down: bool, keysym: u32 },
    CutText(String),
    Refresh,
    Close,
}

#[derive(Clone)]
pub enum VncOutput {
    Ready {
        width: u16,
        height: u16,
        name: String,
    },
    Frame(FrameBuffer),
    Bell,
    CutText(String),
}

struct Conn {
    stream: TcpStream,
    format: PixelFormat,
    frame: FrameBuffer,
    name: String,
}

impl Conn {
    async fn connect(config: &VncClientConfig) -> Result<Self> {
        let (dial_host, dial_port) = config.dial_addr();
        let addr = format!("{dial_host}:{dial_port}");
        let mut stream = TcpStream::connect(&addr).await?;
        stream.set_nodelay(true)?;

        let mut version = [0u8; 12];
        stream.read_exact(&mut version).await?;
        if !version.starts_with(b"RFB ") {
            return Err(Error::Protocol(format!(
                "bad protocol version: {:?}",
                String::from_utf8_lossy(&version)
            )));
        }
        stream.write_all(RFB_VERSION).await?;

        authenticate(&mut stream, config).await?;

        stream.write_all(&[u8::from(config.shared)]).await?;

        let mut header = [0u8; 24];
        stream.read_exact(&mut header).await?;
        let width = u16::from_be_bytes([header[0], header[1]]);
        let height = u16::from_be_bytes([header[2], header[3]]);
        let format = PixelFormat::read(&header[4..20])
            .ok_or_else(|| Error::Protocol("bad server pixel format".into()))?;
        let name_len = u32::from_be_bytes([header[20], header[21], header[22], header[23]]) as usize;
        let mut name = vec![0u8; name_len];
        if name_len > 0 {
            stream.read_exact(&mut name).await?;
        }
        let name = String::from_utf8_lossy(&name).into_owned();

        let mut conn = Self {
            stream,
            format,
            frame: FrameBuffer::new(width, height),
            name,
        };
        // Ask for a format we can paint without conversion surprises.
        conn.set_pixel_format(PixelFormat::RGB888_LE).await?;
        conn.format = PixelFormat::RGB888_LE;
        conn.set_encodings(&[ENC_COPY_RECT, ENC_RAW]).await?;
        conn.request_update(false).await?;
        Ok(conn)
    }

    async fn set_pixel_format(&mut self, format: PixelFormat) -> Result<()> {
        let mut buf = Vec::with_capacity(20);
        buf.push(0); // SetPixelFormat
        buf.extend_from_slice(&[0; 3]);
        format.write(&mut buf);
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn set_encodings(&mut self, encodings: &[i32]) -> Result<()> {
        let mut buf = Vec::with_capacity(4 + encodings.len() * 4);
        buf.push(2); // SetEncodings
        buf.push(0);
        buf.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for &enc in encodings {
            buf.extend_from_slice(&enc.to_be_bytes());
        }
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn request_update(&mut self, incremental: bool) -> Result<()> {
        let mut buf = [0u8; 10];
        buf[0] = 3; // FramebufferUpdateRequest
        buf[1] = u8::from(incremental);
        // x=0,y=0,w,h
        let (w, h) = (self.frame.width, self.frame.height);
        buf[6..8].copy_from_slice(&w.to_be_bytes());
        buf[8..10].copy_from_slice(&h.to_be_bytes());
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn send_pointer(&mut self, x: u16, y: u16, buttons: u8) -> Result<()> {
        let mut buf = [0u8; 6];
        buf[0] = 5;
        buf[1] = buttons;
        buf[2..4].copy_from_slice(&x.to_be_bytes());
        buf[4..6].copy_from_slice(&y.to_be_bytes());
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn send_key(&mut self, down: bool, keysym: u32) -> Result<()> {
        let mut buf = [0u8; 8];
        buf[0] = 4;
        buf[1] = u8::from(down);
        buf[4..8].copy_from_slice(&keysym.to_be_bytes());
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn send_cut_text(&mut self, text: &str) -> Result<()> {
        let bytes = text.as_bytes();
        let mut buf = Vec::with_capacity(8 + bytes.len());
        buf.push(6);
        buf.extend_from_slice(&[0; 3]);
        buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(bytes);
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    async fn handle_server_message(&mut self) -> Result<Option<VncOutput>> {
        let mut kind = [0u8; 1];
        match self.stream.read_exact(&mut kind).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Error::Closed);
            }
            Err(e) => return Err(e.into()),
        }
        match kind[0] {
            0 => {
                self.read_framebuffer_update().await?;
                Ok(Some(VncOutput::Frame(self.frame.clone())))
            }
            1 => {
                // SetColourMapEntries — ignore after our true-colour format.
                let mut hdr = [0u8; 5];
                self.stream.read_exact(&mut hdr).await?;
                let count = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
                let mut skip = vec![0u8; count * 6];
                if !skip.is_empty() {
                    self.stream.read_exact(&mut skip).await?;
                }
                Ok(None)
            }
            2 => Ok(Some(VncOutput::Bell)),
            3 => {
                let mut hdr = [0u8; 7];
                self.stream.read_exact(&mut hdr).await?;
                let len = u32::from_be_bytes([hdr[3], hdr[4], hdr[5], hdr[6]]) as usize;
                let mut text = vec![0u8; len];
                if len > 0 {
                    self.stream.read_exact(&mut text).await?;
                }
                Ok(Some(VncOutput::CutText(
                    String::from_utf8_lossy(&text).into_owned(),
                )))
            }
            other => Err(Error::Protocol(format!("unknown server message {other}"))),
        }
    }

    async fn read_framebuffer_update(&mut self) -> Result<()> {
        let mut hdr = [0u8; 3];
        self.stream.read_exact(&mut hdr).await?;
        let count = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
        for _ in 0..count {
            let mut rh = [0u8; 12];
            self.stream.read_exact(&mut rh).await?;
            let x = u16::from_be_bytes([rh[0], rh[1]]);
            let y = u16::from_be_bytes([rh[2], rh[3]]);
            let w = u16::from_be_bytes([rh[4], rh[5]]);
            let h = u16::from_be_bytes([rh[6], rh[7]]);
            let encoding = i32::from_be_bytes([rh[8], rh[9], rh[10], rh[11]]);
            match encoding {
                ENC_RAW => self.decode_raw(x, y, w, h).await?,
                ENC_COPY_RECT => self.decode_copy_rect(x, y, w, h).await?,
                other => {
                    return Err(Error::Protocol(format!("unsupported encoding {other}")));
                }
            }
        }
        Ok(())
    }

    async fn decode_raw(&mut self, x: u16, y: u16, w: u16, h: u16) -> Result<()> {
        let bpp = self.format.bytes_per_pixel();
        let nbytes = w as usize * h as usize * bpp;
        let mut raw = vec![0u8; nbytes];
        if nbytes > 0 {
            self.stream.read_exact(&mut raw).await?;
        }
        let fw = self.frame.width as usize;
        let pixels = self.frame.pixels_mut();
        let mut row_bgra = vec![0u8; w as usize * 4];
        for row in 0..h as usize {
            let src = &raw[row * w as usize * bpp..(row + 1) * w as usize * bpp];
            decode_pixels(self.format, src, &mut row_bgra);
            let dst_y = y as usize + row;
            let dst_x = x as usize;
            let start = (dst_y * fw + dst_x) * 4;
            pixels[start..start + row_bgra.len()].copy_from_slice(&row_bgra);
        }
        Ok(())
    }

    async fn decode_copy_rect(&mut self, x: u16, y: u16, w: u16, h: u16) -> Result<()> {
        let mut src = [0u8; 4];
        self.stream.read_exact(&mut src).await?;
        let src_x = u16::from_be_bytes([src[0], src[1]]) as usize;
        let src_y = u16::from_be_bytes([src[2], src[3]]) as usize;
        let fw = self.frame.width as usize;
        let (dw, dh) = (w as usize, h as usize);
        let mut block = vec![0u8; dw * dh * 4];
        {
            let pixels = self.frame.pixels.as_ref();
            for row in 0..dh {
                let s = ((src_y + row) * fw + src_x) * 4;
                let d = row * dw * 4;
                block[d..d + dw * 4].copy_from_slice(&pixels[s..s + dw * 4]);
            }
        }
        let pixels = self.frame.pixels_mut();
        for row in 0..dh {
            let d = ((y as usize + row) * fw + x as usize) * 4;
            let s = row * dw * 4;
            pixels[d..d + dw * 4].copy_from_slice(&block[s..s + dw * 4]);
        }
        Ok(())
    }
}

async fn authenticate(stream: &mut TcpStream, config: &VncClientConfig) -> Result<()> {
    let mut count = [0u8; 1];
    stream.read_exact(&mut count).await?;
    if count[0] == 0 {
        // RFB 3.8 failure: reason string.
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await?;
        let n = u32::from_be_bytes(len) as usize;
        let mut reason = vec![0u8; n];
        if n > 0 {
            stream.read_exact(&mut reason).await?;
        }
        return Err(Error::Protocol(format!(
            "server rejected connection: {}",
            String::from_utf8_lossy(&reason)
        )));
    }
    let mut types = vec![0u8; count[0] as usize];
    stream.read_exact(&mut types).await?;

    let prefer = if types.contains(&SEC_VNC) {
        SEC_VNC
    } else if types.contains(&SEC_NONE) {
        SEC_NONE
    } else if types.contains(&SEC_INVALID) {
        return Err(Error::Protocol("server sent no security types".into()));
    } else {
        return Err(Error::UnsupportedSecurity);
    };
    stream.write_all(&[prefer]).await?;

    if prefer == SEC_NONE {
        // RFB 3.8 still sends a SecurityResult for None.
        return read_security_result(stream).await;
    }

    let mut challenge = [0u8; 16];
    stream.read_exact(&mut challenge).await?;
    let key = CredentialKey::password("vnc", "", &config.host, config.port);
    let prompt_text = format!("VNC password for {}:{}", config.host, config.port);

    // Config → vault → ask. A failed attempt ends the TCP session; the UI reconnects.
    let (password, report) = if let Some(password) = config.password.clone() {
        (password, false)
    } else if let Some(saved) = config.prompt.as_ref().and_then(|p| p.saved_credential(&key)) {
        (saved, true)
    } else {
        let asked = config
            .prompt
            .as_ref()
            .and_then(|p| p.ask_credential(&key, &prompt_text))
            .ok_or(Error::Cancelled)?;
        (asked, true)
    };

    let response = encrypt_challenge(&password, &challenge);
    stream.write_all(&response).await?;
    let result = read_security_result(stream).await;
    if report {
        if let Some(prompt) = &config.prompt {
            prompt.credential_result(&key, &password, result.is_ok());
        }
    }
    result
}

async fn read_security_result(stream: &mut TcpStream) -> Result<()> {
    let mut status = [0u8; 4];
    stream.read_exact(&mut status).await?;
    if status == [0, 0, 0, 0] {
        return Ok(());
    }
    // Optional reason in 3.8
    let mut len = [0u8; 4];
    if stream.read_exact(&mut len).await.is_ok() {
        let n = u32::from_be_bytes(len) as usize;
        let mut reason = vec![0u8; n.min(1024)];
        let _ = stream.read_exact(&mut reason).await;
        if !reason.is_empty() {
            return Err(Error::Protocol(format!(
                "auth failed: {}",
                String::from_utf8_lossy(&reason)
            )));
        }
    }
    Err(Error::AuthFailed)
}

/// Connected VNC session that pumps frames until closed.
pub struct VncSession {
    conn: Conn,
}

impl VncSession {
    pub async fn connect(config: VncClientConfig) -> Result<Self> {
        Ok(Self {
            conn: Conn::connect(&config).await?,
        })
    }

    pub fn size(&self) -> (u16, u16) {
        (self.conn.frame.width, self.conn.frame.height)
    }

    pub fn name(&self) -> &str {
        &self.conn.name
    }

    /// Drive the session until `Close` or the server drops.
    pub async fn run(
        mut self,
        mut input: mpsc::UnboundedReceiver<VncInput>,
        output: mpsc::UnboundedSender<VncOutput>,
    ) -> Result<()> {
        let _ = output.send(VncOutput::Ready {
            width: self.conn.frame.width,
            height: self.conn.frame.height,
            name: self.conn.name.clone(),
        });
        // First full frame arrives from the initial non-incremental request.
        loop {
            tokio::select! {
                msg = self.conn.handle_server_message() => {
                    match msg? {
                        Some(out) => {
                            let is_frame = matches!(out, VncOutput::Frame(_));
                            if output.send(out).is_err() {
                                return Ok(());
                            }
                            if is_frame {
                                self.conn.request_update(true).await?;
                            }
                        }
                        None => {}
                    }
                }
                event = input.recv() => {
                    match event {
                        None | Some(VncInput::Close) => return Ok(()),
                        Some(VncInput::Pointer { x, y, buttons }) => {
                            self.conn.send_pointer(x, y, buttons).await?;
                        }
                        Some(VncInput::Key { down, keysym }) => {
                            self.conn.send_key(down, keysym).await?;
                        }
                        Some(VncInput::CutText(text)) => {
                            self.conn.send_cut_text(&text).await?;
                        }
                        Some(VncInput::Refresh) => {
                            self.conn.request_update(false).await?;
                        }
                    }
                }
            }
        }
    }
}
