//! IronRDP session: connect, paint frames, forward pointer / keyboard.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use gale_core::vault::CredentialKey;
use gale_core::{RdpConfig, SessionProfile};
use ironrdp::connector::{self, ConnectionResult, Credentials};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::error::{Error, Result};
use crate::input::InputMapper;
use crate::tls;

/// Asks for the RDP password when none is configured / vault miss.
pub trait RdpPrompt: Send + Sync {
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
pub struct RdpClientConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub domain: Option<String>,
    pub password: Option<String>,
    pub prompt: Option<Arc<dyn RdpPrompt>>,
    pub width: u16,
    pub height: u16,
    /// Dial this endpoint instead of `host:port` (SSH local tunnel).
    /// TLS / CredSSP still use `host` as the logical server name.
    pub dial: Option<(String, u16)>,
}

impl RdpClientConfig {
    pub fn from_profile(profile: &SessionProfile) -> Result<Self> {
        let rdp = profile.rdp.as_ref().ok_or(Error::MissingConfig)?;
        Ok(Self::from_rdp(rdp))
    }

    pub fn from_rdp(rdp: &RdpConfig) -> Self {
        Self {
            host: rdp.host.clone(),
            port: rdp.port,
            username: rdp.username.clone(),
            domain: rdp.domain.clone(),
            password: rdp.password.clone(),
            prompt: None,
            width: 1280,
            height: 800,
            dial: None,
        }
    }

    pub fn with_prompt(mut self, prompt: Arc<dyn RdpPrompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }

    pub fn with_dial(mut self, host: impl Into<String>, port: u16) -> Self {
        self.dial = Some((host.into(), port));
        self
    }

    fn credential_key(&self) -> CredentialKey {
        CredentialKey::password("rdp", &self.username, &self.host, self.port)
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
    fn from_rgba(width: u16, height: u16, rgba: &[u8]) -> Self {
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        for (src, dst) in rgba.chunks_exact(4).zip(pixels.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = if src[3] == 0 { 255 } else { src[3] };
        }
        Self {
            width,
            height,
            pixels: pixels.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum RdpInput {
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { down: bool, keysym: u32 },
    CutText(String),
    Refresh,
    Close,
}

#[derive(Clone)]
pub enum RdpOutput {
    Ready {
        width: u16,
        height: u16,
        name: String,
    },
    Frame(FrameBuffer),
    CutText(String),
}

type UpgradedFramed = ironrdp_blocking::Framed<tls::TlsStream>;

pub struct RdpSession {
    connection: ConnectionResult,
    framed: UpgradedFramed,
    label: String,
}

impl RdpSession {
    pub fn connect(config: RdpClientConfig) -> Result<Self> {
        let password = resolve_password(&config)?;
        let connector_config = build_connector_config(&config, password.clone());
        let (dial_host, dial_port) = config.dial_addr();
        match connect_inner(connector_config, &config.host, &dial_host, dial_port) {
            Ok((connection, framed)) => {
                if let Some(prompt) = &config.prompt {
                    prompt.credential_result(&config.credential_key(), &password, true);
                }
                Ok(Self {
                    connection,
                    framed,
                    label: format!("{}@{}:{}", config.username, config.host, config.port),
                })
            }
            Err(err) => {
                if let Some(prompt) = &config.prompt {
                    prompt.credential_result(&config.credential_key(), &password, false);
                }
                Err(err)
            }
        }
    }

    /// Blocking run loop. Call from `spawn_blocking` / a dedicated thread.
    pub fn run(
        mut self,
        mut input_rx: mpsc::UnboundedReceiver<RdpInput>,
        output_tx: mpsc::UnboundedSender<RdpOutput>,
    ) -> Result<()> {
        let width = self.connection.desktop_size.width;
        let height = self.connection.desktop_size.height;
        let _ = output_tx.send(RdpOutput::Ready {
            width,
            height,
            name: self.label.clone(),
        });

        let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
        let mut active_stage = ActiveStageBuilder {
            static_channels: self.connection.static_channels,
            user_channel_id: self.connection.user_channel_id,
            io_channel_id: self.connection.io_channel_id,
            message_channel_id: self.connection.message_channel_id,
            share_id: self.connection.share_id,
            compression_type: self.connection.compression_type,
            enable_server_pointer: self.connection.enable_server_pointer,
            pointer_software_rendering: self.connection.pointer_software_rendering,
        }
        .build();
        let mut mapper = InputMapper::new();

        // Short read timeout so we can poll UI input between PDUs.
        self.framed
            .get_inner()
            .0
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(30)))
            .ok();

        loop {
            while let Ok(input) = input_rx.try_recv() {
                match input {
                    RdpInput::Close => return Ok(()),
                    RdpInput::CutText(text) => {
                        // One character per FastPath PDU + a short gap so xrdp
                        // (and similar) can process scancodes instead of dropping
                        // a Unicode/scancode burst.
                        for ch in text.chars() {
                            let events = mapper.map_paste_char(ch);
                            if events.is_empty() {
                                continue;
                            }
                            let outputs = active_stage
                                .process_fastpath_input(&mut image, &events)
                                .map_err(|e| anyhow::anyhow!("paste input: {e}"))?;
                            handle_outputs(
                                &mut self.framed,
                                &mut image,
                                &output_tx,
                                outputs,
                                false,
                            )?;
                            std::thread::sleep(Duration::from_millis(8));
                        }
                    }
                    other => {
                        let events = mapper.map(&other);
                        if events.is_empty() {
                            continue;
                        }
                        let outputs = active_stage
                            .process_fastpath_input(&mut image, &events)
                            .map_err(|e| anyhow::anyhow!("input: {e}"))?;
                        handle_outputs(
                            &mut self.framed,
                            &mut image,
                            &output_tx,
                            outputs,
                            false,
                        )?;
                    }
                }
            }

            let (action, payload) = match self.framed.read_pdu() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(Error::Io(e)),
            };

            trace!(?action, len = payload.len(), "RDP PDU");
            let outputs = active_stage
                .process(&mut image, action, &payload)
                .map_err(|e| anyhow::anyhow!("process: {e}"))?;
            if handle_outputs(&mut self.framed, &mut image, &output_tx, outputs, true)? {
                return Ok(());
            }
        }
    }
}

fn handle_outputs(
    framed: &mut UpgradedFramed,
    image: &mut DecodedImage,
    output_tx: &mpsc::UnboundedSender<RdpOutput>,
    outputs: Vec<ActiveStageOutput>,
    emit_frame: bool,
) -> Result<bool> {
    let mut terminated = false;
    let mut dirty = false;
    for out in outputs {
        match out {
            ActiveStageOutput::ResponseFrame(frame) if !frame.is_empty() => {
                framed.write_all(&frame).map_err(Error::Io)?;
            }
            ActiveStageOutput::GraphicsUpdate(_) => dirty = true,
            ActiveStageOutput::Terminate(_) => terminated = true,
            _ => {}
        }
    }
    if emit_frame && dirty {
        let frame = FrameBuffer::from_rgba(image.width(), image.height(), image.data());
        let _ = output_tx.send(RdpOutput::Frame(frame));
    }
    Ok(terminated)
}

fn resolve_password(config: &RdpClientConfig) -> Result<String> {
    if let Some(pw) = config.password.as_ref().filter(|s| !s.is_empty()) {
        return Ok(pw.clone());
    }
    let key = config.credential_key();
    if let Some(prompt) = &config.prompt {
        if let Some(saved) = prompt.saved_credential(&key) {
            return Ok(saved);
        }
        let label = format!("Password for rdp://{}@{}:{}", config.username, config.host, config.port);
        if let Some(pw) = prompt.ask_credential(&key, &label) {
            return Ok(pw);
        }
    }
    Err(Error::PasswordRequired)
}

fn build_connector_config(config: &RdpClientConfig, password: String) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password,
        },
        domain: config.domain.clone(),
        // Offer both PROTOCOL_SSL and PROTOCOL_HYBRID so xrdp / Windows can pick.
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: config.width,
            height: config.height,
        },
        bitmap: None,
        client_build: 0,
        client_name: "GaleTerm".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "ios")]
        platform: MajorPlatformType::IOS,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "android")]
        platform: MajorPlatformType::ANDROID,
        #[cfg(any(
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
        ))]
        platform: MajorPlatformType::UNIX,
        #[cfg(not(any(
            windows,
            target_os = "macos",
            target_os = "ios",
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
        )))]
        platform: MajorPlatformType::UNSPECIFIED,
        enable_server_pointer: true,
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        compression_type: Some(CompressionType::Rdp61),
        pointer_software_rendering: true,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

/// `server_name` is the logical host for TLS SNI / CredSSP; dial uses `dial_*`.
fn connect_inner(
    config: connector::Config,
    server_name: &str,
    dial_host: &str,
    dial_port: u16,
) -> Result<(ConnectionResult, UpgradedFramed)> {
    let addr = (dial_host, dial_port)
        .to_socket_addrs()
        .map_err(Error::Io)?
        .next()
        .ok_or_else(|| {
            Error::Other(anyhow::anyhow!("no address for {dial_host}:{dial_port}"))
        })?;
    debug!(%addr, server = %server_name, "RDP TCP connect");
    let tcp = TcpStream::connect(addr).map_err(Error::Io)?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(Error::Io)?;
    let client_addr = tcp.local_addr().map_err(Error::Io)?;

    let mut framed = ironrdp_blocking::Framed::new(tcp);
    let mut connector = connector::ClientConnector::new(config, client_addr);
    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut connector)
        .map_err(|e| Error::Other(anyhow::anyhow!("connect begin: {e}")))?;

    let initial = framed.into_inner_no_leftover();
    // Logical hostname for SNI / CredSSP even when dialing a local tunnel.
    let (tls_stream, server_public_key) = tls::upgrade(initial, server_name)
        .map_err(|e| Error::Other(anyhow::anyhow!("TLS upgrade: {e:#}")))?;
    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);
    let mut upgraded_framed = ironrdp_blocking::Framed::new(tls_stream);

    let mut network_client = ReqwestNetworkClient;
    let connection_result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut network_client,
        server_name.to_owned().into(),
        server_public_key,
        None,
    )
    .map_err(|e| Error::Other(anyhow::anyhow!("connect finalize: {e}")))?;

    Ok((connection_result, upgraded_framed))
}

// Silence unused Read import used via Framed bounds.
#[allow(dead_code)]
fn _read_bound<R: Read>(_: &R) {}
#[allow(dead_code)]
fn _write_bound<W: Write>(_: &W) {}
