//! CLIPRDR backend: Unicode text + DIB/DIBV5 images via IronRDP.

use std::sync::{Arc, Mutex};

use ironrdp::cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp::core::{impl_as_any, IntoOwned};
use ironrdp_cliprdr_format::bitmap::{dib_to_png, dibv5_to_png, png_to_cf_dib, png_to_cf_dibv5};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::client::RdpOutput;

/// Commands from the CLIPRDR backend into the session loop.
pub(crate) enum CliprdrCmd {
    Protocol(ClipboardMessage),
    RemoteText(String),
    /// PNG bytes for the system clipboard.
    RemoteImage(Vec<u8>),
}

#[derive(Clone, Default)]
enum LocalPayload {
    #[default]
    Empty,
    Text(String),
    /// PNG-encoded image.
    Png(Vec<u8>),
}

/// Shared state: last payload we offered to the remote (for FormatDataRequest).
#[derive(Clone, Default)]
pub(crate) struct LocalClipboard {
    inner: Arc<Mutex<LocalPayload>>,
}

impl LocalClipboard {
    pub fn set_text(&self, text: String) {
        if let Ok(mut g) = self.inner.lock() {
            *g = LocalPayload::Text(text);
        }
    }

    pub fn set_png(&self, png: Vec<u8>) {
        if let Ok(mut g) = self.inner.lock() {
            *g = LocalPayload::Png(png);
        }
    }

    fn get(&self) -> LocalPayload {
        self.inner
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

pub(crate) fn unicode_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
}

fn dib_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_DIB)
}

fn dibv5_format() -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::CF_DIBV5)
}

fn formats_for_payload(payload: &LocalPayload) -> Vec<ClipboardFormat> {
    match payload {
        LocalPayload::Empty | LocalPayload::Text(_) => vec![unicode_format()],
        LocalPayload::Png(_) => vec![dibv5_format(), dib_format(), unicode_format()],
    }
}

/// OS-agnostic CLIPRDR backend used by GaleTerm (text + bitmap).
pub(crate) struct GaleCliprdrBackend {
    cmd_tx: mpsc::UnboundedSender<CliprdrCmd>,
    local: LocalClipboard,
    /// Format id of the most recent initiate_paste we sent.
    pending_paste: Option<ClipboardFormatId>,
    temp_dir: String,
}

impl std::fmt::Debug for GaleCliprdrBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaleCliprdrBackend").finish_non_exhaustive()
    }
}

impl_as_any!(GaleCliprdrBackend);

impl GaleCliprdrBackend {
    pub fn new(cmd_tx: mpsc::UnboundedSender<CliprdrCmd>, local: LocalClipboard) -> Self {
        let temp_dir = std::env::temp_dir()
            .join("gale-rdp-cliprdr")
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::create_dir_all(&temp_dir);
        Self {
            cmd_tx,
            local,
            pending_paste: None,
            temp_dir,
        }
    }

    fn send(&self, cmd: CliprdrCmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

impl CliprdrBackend for GaleCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        &self.temp_dir
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        debug!("CLIPRDR ready");
    }

    fn on_request_format_list(&mut self) {
        let formats = formats_for_payload(&self.local.get());
        self.send(CliprdrCmd::Protocol(ClipboardMessage::SendInitiateCopy(
            formats,
        )));
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        debug!(?capabilities, "CLIPRDR capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let ids: Vec<_> = available_formats.iter().map(|f| f.id()).collect();
        let format = if ids.contains(&ClipboardFormatId::CF_DIBV5) {
            ClipboardFormatId::CF_DIBV5
        } else if ids.contains(&ClipboardFormatId::CF_DIB) {
            ClipboardFormatId::CF_DIB
        } else if ids.contains(&ClipboardFormatId::CF_UNICODETEXT) {
            ClipboardFormatId::CF_UNICODETEXT
        } else if ids.contains(&ClipboardFormatId::CF_TEXT) {
            ClipboardFormatId::CF_TEXT
        } else {
            debug!(?available_formats, "remote clipboard has no supported formats");
            return;
        };
        self.pending_paste = Some(format);
        self.send(CliprdrCmd::Protocol(ClipboardMessage::SendInitiatePaste(
            format,
        )));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let payload = self.local.get();
        let response = match (request.format, payload) {
            (ClipboardFormatId::CF_UNICODETEXT, LocalPayload::Text(text)) => {
                FormatDataResponse::new_unicode_string(&normalize_crlf(&text)).into_owned()
            }
            (ClipboardFormatId::CF_TEXT, LocalPayload::Text(text)) => {
                FormatDataResponse::new_string(&normalize_crlf(&text)).into_owned()
            }
            (ClipboardFormatId::CF_DIBV5, LocalPayload::Png(png)) => {
                match png_to_cf_dibv5(&png) {
                    Ok(dib) => FormatDataResponse::new_data(dib).into_owned(),
                    Err(err) => {
                        warn!(%err, "png→DIBV5 failed");
                        FormatDataResponse::new_error().into_owned()
                    }
                }
            }
            (ClipboardFormatId::CF_DIB, LocalPayload::Png(png)) => match png_to_cf_dib(&png) {
                Ok(dib) => FormatDataResponse::new_data(dib).into_owned(),
                Err(err) => {
                    warn!(%err, "png→DIB failed");
                    FormatDataResponse::new_error().into_owned()
                }
            },
            // Remote asked for text while we only have an image (or vice versa).
            (ClipboardFormatId::CF_UNICODETEXT | ClipboardFormatId::CF_TEXT, _) => {
                FormatDataResponse::new_unicode_string("").into_owned()
            }
            _ => {
                warn!(?request.format, "unsupported CLIPRDR format request");
                FormatDataResponse::new_error().into_owned()
            }
        };
        self.send(CliprdrCmd::Protocol(ClipboardMessage::SendFormatData(
            response,
        )));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            warn!("CLIPRDR format data response error");
            self.pending_paste = None;
            return;
        }
        let format = self.pending_paste.take();
        match format {
            Some(ClipboardFormatId::CF_DIBV5) => match dibv5_to_png(response.data()) {
                Ok(png) => self.send(CliprdrCmd::RemoteImage(png)),
                Err(err) => warn!(%err, "DIBV5→png failed"),
            },
            Some(ClipboardFormatId::CF_DIB) => match dib_to_png(response.data()) {
                Ok(png) => self.send(CliprdrCmd::RemoteImage(png)),
                Err(err) => warn!(%err, "DIB→png failed"),
            },
            _ => {
                let text = response
                    .to_unicode_string()
                    .or_else(|_| response.to_string())
                    .unwrap_or_default();
                let text = text.trim_end_matches('\0').replace("\r\n", "\n");
                if !text.is_empty() {
                    self.send(CliprdrCmd::RemoteText(text));
                }
            }
        }
    }

    fn on_file_contents_request(&mut self, _: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}

fn normalize_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Apply a CLIPRDR protocol message; returns frames to write (if any).
pub(crate) fn process_clipboard_message(
    active_stage: &mut ironrdp::session::ActiveStage,
    event: ClipboardMessage,
) -> anyhow::Result<Vec<ironrdp::session::ActiveStageOutput>> {
    use ironrdp::cliprdr::CliprdrClient;

    let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() else {
        warn!("CLIPRDR event but channel missing");
        return Ok(Vec::new());
    };

    let svc_messages = match event {
        ClipboardMessage::SendInitiateCopy(formats) => cliprdr
            .initiate_copy(&formats)
            .map_err(|e| anyhow::anyhow!("CLIPRDR initiate_copy: {e}"))?,
        ClipboardMessage::SendFormatData(response) => cliprdr
            .submit_format_data(response)
            .map_err(|e| anyhow::anyhow!("CLIPRDR submit_format_data: {e}"))?,
        ClipboardMessage::SendInitiatePaste(format) => cliprdr
            .initiate_paste(format)
            .map_err(|e| anyhow::anyhow!("CLIPRDR initiate_paste: {e}"))?,
        ClipboardMessage::Error(err) => {
            warn!(%err, "CLIPRDR backend error");
            return Ok(Vec::new());
        }
        other => {
            debug!(?other, "ignoring unsupported CLIPRDR message");
            return Ok(Vec::new());
        }
    };

    let frame = active_stage
        .process_svc_processor_messages(svc_messages)
        .map_err(|e| anyhow::anyhow!("CLIPRDR encode: {e}"))?;
    Ok(vec![
        ironrdp::session::ActiveStageOutput::ResponseFrame(frame),
    ])
}

pub(crate) fn offer_local_text(
    active_stage: &mut ironrdp::session::ActiveStage,
    local: &LocalClipboard,
    text: &str,
) -> anyhow::Result<Vec<ironrdp::session::ActiveStageOutput>> {
    local.set_text(text.to_owned());
    process_clipboard_message(
        active_stage,
        ClipboardMessage::SendInitiateCopy(formats_for_payload(&local.get())),
    )
}

pub(crate) fn offer_local_png(
    active_stage: &mut ironrdp::session::ActiveStage,
    local: &LocalClipboard,
    png: Vec<u8>,
) -> anyhow::Result<Vec<ironrdp::session::ActiveStageOutput>> {
    local.set_png(png);
    process_clipboard_message(
        active_stage,
        ClipboardMessage::SendInitiateCopy(formats_for_payload(&local.get())),
    )
}

pub(crate) fn emit_remote_text(output_tx: &mpsc::UnboundedSender<RdpOutput>, text: String) {
    let _ = output_tx.send(RdpOutput::CutText(text));
}

pub(crate) fn emit_remote_image(output_tx: &mpsc::UnboundedSender<RdpOutput>, png: Vec<u8>) {
    let _ = output_tx.send(RdpOutput::CutImage(png));
}
