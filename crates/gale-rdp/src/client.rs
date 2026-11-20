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

use crate::cliprdr::{self, CliprdrCmd, GaleCliprdrBackend, LocalClipboard};
use crate::error::{Error, Result};
use crate::input::InputMapper;
use crate::tls;

/// Details shown when the RDP server TLS certificate is not system-trusted.
#[derive(Debug, Clone)]
pub struct RdpCertInfo {
    pub host: String,
    pub port: u16,
    /// `SHA256:` followed by colon-separated hex.
    pub fingerprint: String,
    /// Why the system trust store rejected it.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdpCertDecision {
    Reject,
    AcceptOnce,
    AcceptAndSave,
}

/// Asks for the RDP password / TLS trust when needed.
pub trait RdpPrompt: Send + Sync {
    fn ask_password(&self, prompt: &str) -> Option<String>;

    fn ask_credential(&self, _key: &CredentialKey, prompt: &str) -> Option<String> {
        self.ask_password(prompt)
    }

    fn saved_credential(&self, _key: &CredentialKey) -> Option<String> {
        None
    }

    fn credential_result(&self, _key: &CredentialKey, _secret: &str, _accepted: bool) {}

    /// Untrusted TLS certificate — default rejects (CLI / headless).
    fn confirm_certificate(&self, _info: &RdpCertInfo) -> RdpCertDecision {
        RdpCertDecision::Reject
    }
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
    /// PNG bytes to offer over CLIPRDR as CF_DIB / CF_DIBV5.
    CutImage(Vec<u8>),
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
    /// PNG bytes from a remote CF_DIB / CF_DIBV5 copy.
    CutImage(Vec<u8>),
}

type UpgradedFramed = ironrdp_blocking::Framed<tls::TlsStream>;

pub struct RdpSession {
    connection: ConnectionResult,
    framed: UpgradedFramed,
    label: String,
    clip_rx: mpsc::UnboundedReceiver<CliprdrCmd>,
    local_clipboard: LocalClipboard,
}

impl RdpSession {
    pub fn connect(config: RdpClientConfig) -> Result<Self> {
        let password = resolve_password(&config)?;
        let connector_config = build_connector_config(&config, password.clone());
        let (dial_host, dial_port) = config.dial_addr();
        match connect_inner(
            connector_config,
            &config.host,
            config.port,
            &dial_host,
            dial_port,
            config.prompt.clone(),
        ) {
            Ok((connection, framed, clip_rx, local_clipboard)) => {
                if let Some(prompt) = &config.prompt {
                    prompt.credential_result(&config.credential_key(), &password, true);
                }
                Ok(Self {
                    connection,
                    framed,
                    label: format!("{}@{}:{}", config.username, config.host, config.port),
                    clip_rx,
                    local_clipboard,
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
        let local_clipboard = self.local_clipboard.clone();

        // Short read timeout so we can poll UI input between PDUs.
        self.framed
            .get_inner()
            .0
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(30)))
            .ok();

        loop {
            while let Ok(cmd) = self.clip_rx.try_recv() {
                match cmd {
                    CliprdrCmd::Protocol(msg) => {
                        let outputs = cliprdr::process_clipboard_message(&mut active_stage, msg)?;
                        handle_outputs(
                            &mut self.framed,
                            &mut image,
                            &output_tx,
                            outputs,
                            false,
                        )?;
                    }
                    CliprdrCmd::RemoteText(text) => {
                        cliprdr::emit_remote_text(&output_tx, text);
                    }
                    CliprdrCmd::RemoteImage(png) => {
                        cliprdr::emit_remote_image(&output_tx, png);
                    }
                }
            }

            while let Ok(input) = input_rx.try_recv() {
                match input {
                    RdpInput::Close => return Ok(()),
                    RdpInput::CutImage(png) => {
                        match cliprdr::offer_local_png(
                            &mut active_stage,
                            &local_clipboard,
                            png,
                        ) {
                            Ok(outputs) => {
                                handle_outputs(
                                    &mut self.framed,
                                    &mut image,
                                    &output_tx,
                                    outputs,
                                    false,
                                )?;
                                send_ctrl_v(
                                    &mut active_stage,
                                    &mut mapper,
                                    &mut image,
                                    &mut self.framed,
                                    &output_tx,
                                )?;
                            }
                            Err(err) => {
                                tracing::debug!(%err, "CLIPRDR image paste unavailable");
                            }
                        }
                    }
                    RdpInput::CutText(text) => {
                        // Prefer CLIPRDR: advertise text, then Ctrl+V into the remote app.
                        // Fall back to scancode typing if the channel is unavailable.
                        let used_cliprdr = match cliprdr::offer_local_text(
                            &mut active_stage,
                            &local_clipboard,
                            &text,
                        ) {
                            Ok(outputs) => {
                                handle_outputs(
                                    &mut self.framed,
                                    &mut image,
                                    &output_tx,
                                    outputs,
                                    false,
                                )?;
                                true
                            }
                            Err(err) => {
                                tracing::debug!(%err, "CLIPRDR paste unavailable; using scancodes");
                                false
                            }
                        };

                        if used_cliprdr {
                            send_ctrl_v(
                                &mut active_stage,
                                &mut mapper,
                                &mut image,
                                &mut self.framed,
                                &output_tx,
                            )?;
                        } else {
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
            let outputs = match active_stage.process(&mut image, action, &payload) {
                Ok(outputs) => outputs,
                Err(err) => {
                    // xrdp (and some Windows builds) emit RDPSND / channel PDUs that
                    // IronRDP 0.9 cannot decode (e.g. msgType=0x27). Treat as non-fatal
                    // so graphics / CLIPRDR / RDPDR keep working.
                    let head = payload
                        .iter()
                        .take(24)
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    tracing::warn!(
                        error = %err,
                        ?action,
                        len = payload.len(),
                        %head,
                        "ignoring RDP PDU process error"
                    );
                    continue;
                }
            };
            if handle_outputs(&mut self.framed, &mut image, &output_tx, outputs, true)? {
                return Ok(());
            }
        }
    }
}

fn send_ctrl_v(
    active_stage: &mut ironrdp::session::ActiveStage,
    mapper: &mut InputMapper,
    image: &mut DecodedImage,
    framed: &mut UpgradedFramed,
    output_tx: &mpsc::UnboundedSender<RdpOutput>,
) -> Result<()> {
    for (down, keysym) in [
        (true, 0xffe3_u32),
        (true, 0x0076),
        (false, 0x0076),
        (false, 0xffe3),
    ] {
        let events = mapper.map(&RdpInput::Key { down, keysym });
        if events.is_empty() {
            continue;
        }
        let outputs = active_stage
            .process_fastpath_input(image, &events)
            .map_err(|e| anyhow::anyhow!("paste chord: {e}"))?;
        handle_outputs(framed, image, output_tx, outputs, false)?;
    }
    Ok(())
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
        enable_audio_playback: true,
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

/// Sandboxed folder exposed to the remote as drive `Gale` (RDPDR).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn default_rdp_share_dir() -> Option<String> {
    let dir = dirs::data_dir()?.join("gale-term").join("rdp-share");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.to_string_lossy().into_owned())
}

/// `server_name` / `server_port` are the logical host for TLS SNI, CredSSP, and
/// known-cert storage; dial uses `dial_*` (may be a local tunnel).
fn connect_inner(
    config: connector::Config,
    server_name: &str,
    server_port: u16,
    dial_host: &str,
    dial_port: u16,
    prompt: Option<Arc<dyn RdpPrompt>>,
) -> Result<(
    ConnectionResult,
    UpgradedFramed,
    mpsc::UnboundedReceiver<CliprdrCmd>,
    LocalClipboard,
)> {
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

    let (clip_tx, clip_rx) = mpsc::unbounded_channel();
    let local_clipboard = LocalClipboard::default();
    let backend = GaleCliprdrBackend::new(clip_tx, local_clipboard.clone());

    let mut framed = ironrdp_blocking::Framed::new(tcp);
    let mut connector = connector::ClientConnector::new(config, client_addr)
        .with_static_channel(ironrdp::cliprdr::Cliprdr::new(Box::new(backend)))
        .with_static_channel(ironrdp::rdpsnd::client::Rdpsnd::new(Box::new(
            ironrdp_rdpsnd_native::cpal::RdpsndBackend::new(),
        )));
    // RDPDR: share a sandboxed local folder (macOS/Linux). Appears as "Gale on GaleTerm".
    // Spec (MS-RDPEFS) expects RDPSND advertised alongside RDPDR.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(share) = default_rdp_share_dir() {
        debug!(%share, "RDPDR drive share");
        connector = connector.with_static_channel(
            ironrdp::rdpdr::Rdpdr::new(
                Box::new(ironrdp_rdpdr_native::backend::NixRdpdrBackend::new(share)),
                "GaleTerm".into(),
            )
            .with_drives(Some(vec![(1, "Gale".into())])),
        );
    }
    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut connector)
        .map_err(|e| Error::Other(anyhow::anyhow!("connect begin: {e}")))?;

    let initial = framed.into_inner_no_leftover();
    let (tls_stream, server_public_key) =
        tls::upgrade(initial, server_name, server_port, prompt)
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

    Ok((connection_result, upgraded_framed, clip_rx, local_clipboard))
}

// Silence unused Read import used via Framed bounds.
#[allow(dead_code)]
fn _read_bound<R: Read>(_: &R) {}
#[allow(dead_code)]
fn _write_bound<W: Write>(_: &W) {}
