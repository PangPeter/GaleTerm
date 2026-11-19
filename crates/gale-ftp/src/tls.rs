//! TLS for FTPS: a stream that is either plain TCP or rustls, and a
//! certificate verifier that falls back to asking the user (TOFU) when the
//! system trust store does not vouch for the server.

use std::collections::HashSet;
use std::fmt;
use std::io::Write as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};

/// Details shown to the user when a server certificate is not trusted.
#[derive(Debug, Clone)]
pub struct CertInfo {
    pub host: String,
    pub port: u16,
    /// `SHA256:` followed by colon-separated hex.
    pub fingerprint: String,
    /// Why the system trust store rejected it.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertDecision {
    Reject,
    AcceptOnce,
    AcceptAndSave,
}

/// Interactive questions the FTP client may need answered.
pub trait FtpPrompt: Send + Sync {
    fn ask_password(&self, prompt: &str) -> Option<String>;
    fn confirm_certificate(&self, info: &CertInfo) -> CertDecision;
}

pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Where fingerprints accepted with "save" are kept (`host:port SHA256:…` lines).
pub fn default_known_certs_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gale-term").join("ftp_known_certs"))
}

pub(crate) fn fingerprint(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    let hex: Vec<String> = digest.iter().map(|b| format!("{b:02X}")).collect();
    format!("SHA256:{}", hex.join(":"))
}

struct Verifier {
    webpki: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    host: String,
    port: u16,
    /// Fingerprints accepted for this client (shared by control and data
    /// connections, and by forked transfer connections).
    trusted: Mutex<HashSet<String>>,
    known_path: Option<PathBuf>,
    prompt: Option<Arc<dyn FtpPrompt>>,
}

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifier")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    fn host_key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn is_known(&self, fp: &str) -> bool {
        let Some(path) = &self.known_path else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let host = self.host_key();
        text.lines().any(|line| {
            let mut parts = line.split_whitespace();
            parts.next() == Some(host.as_str()) && parts.next() == Some(fp)
        })
    }

    fn remember(&self, fp: &str) {
        let Some(path) = &self.known_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{} {fp}", self.host_key()));
        if let Err(err) = written {
            tracing::warn!(?err, "failed to save FTPS certificate fingerprint");
        }
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let fp = fingerprint(end_entity.as_ref());
        if self.trusted.lock().is_ok_and(|t| t.contains(&fp)) {
            return Ok(ServerCertVerified::assertion());
        }
        let err = match &self.webpki {
            Some(webpki) => match webpki.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            ) {
                Ok(ok) => return Ok(ok),
                Err(err) => err,
            },
            None => TlsError::General("no trusted root certificates available".into()),
        };

        let decision = if self.is_known(&fp) {
            CertDecision::AcceptOnce
        } else if let Some(prompt) = &self.prompt {
            prompt.confirm_certificate(&CertInfo {
                host: self.host.clone(),
                port: self.port,
                fingerprint: fp.clone(),
                reason: err.to_string(),
            })
        } else {
            CertDecision::Reject
        };
        match decision {
            CertDecision::Reject => Err(err),
            CertDecision::AcceptOnce | CertDecision::AcceptAndSave => {
                if decision == CertDecision::AcceptAndSave {
                    self.remember(&fp);
                }
                if let Ok(mut trusted) = self.trusted.lock() {
                    trusted.insert(fp);
                }
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Client config shared by every connection of one FTP client, so the data
/// channel can resume the control channel's TLS session (many servers
/// require that) and an accepted certificate is only asked about once.
pub(crate) fn client_config(
    host: &str,
    port: u16,
    known_path: Option<PathBuf>,
    prompt: Option<Arc<dyn FtpPrompt>>,
) -> crate::Result<Arc<ClientConfig>> {
    let provider = Arc::new(ring::default_provider());
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(native.certs);
    let webpki = if roots.is_empty() {
        None
    } else {
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .ok()
    };
    let verifier = Arc::new(Verifier {
        webpki,
        provider: provider.clone(),
        host: host.to_string(),
        port,
        trusted: Mutex::new(HashSet::new()),
        known_path,
        prompt,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| crate::Error::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    #[test]
    fn fingerprint_is_colon_separated_sha256() {
        let fp = fingerprint(b"abc");
        assert!(fp.starts_with("SHA256:BA:78:16:BF"));
        assert_eq!(fp.len(), "SHA256:".len() + 32 * 3 - 1);
    }
}
