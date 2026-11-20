//! TLS upgrade for the RDP security handshake.
//!
//! Tries the system trust store first; on failure falls back to a saved
//! fingerprint file or an interactive [`RdpPrompt::confirm_certificate`]
//! (same TOFU flow as FTPS).

use std::collections::HashSet;
use std::fmt;
use std::io::Write as _;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{
    ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use sha2::{Digest, Sha256};
use x509_cert::der::Decode as _;

use crate::client::{RdpCertDecision, RdpCertInfo, RdpPrompt};

pub(crate) type TlsStream = rustls::StreamOwned<ClientConnection, TcpStream>;

/// Where fingerprints accepted with "save" are kept (`host:port SHA256:…` lines).
pub fn default_known_certs_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gale-term").join("rdp_known_certs"))
}

pub(crate) fn fingerprint(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    let hex: Vec<String> = digest.iter().map(|b| format!("{b:02X}")).collect();
    format!("SHA256:{}", hex.join(":"))
}

pub(crate) fn upgrade(
    stream: TcpStream,
    server_name: &str,
    port: u16,
    prompt: Option<Arc<dyn RdpPrompt>>,
) -> anyhow::Result<(TlsStream, Vec<u8>)> {
    // sspi/reqwest may also pull aws-lc-rs; pick one process-wide provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut config = build_client_config(server_name, port, prompt)?;
    // CredSSP does not support TLS session resumption.
    config.resumption = rustls::client::Resumption::disabled();
    let config = Arc::new(config);

    let name = ServerName::try_from(server_name.to_owned()).context("invalid TLS server name")?;
    let client = ClientConnection::new(config, name).context("TLS client")?;
    let mut tls = rustls::StreamOwned::new(client, stream);
    tls.flush().context("TLS handshake flush")?;

    let cert = tls
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .context("peer certificate missing")?;
    let public_key = extract_public_key(cert).context("extract server public key")?;
    Ok((tls, public_key))
}

fn build_client_config(
    host: &str,
    port: u16,
    prompt: Option<Arc<dyn RdpPrompt>>,
) -> anyhow::Result<ClientConfig> {
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
        known_path: default_known_certs_path(),
        prompt,
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(config)
}

fn extract_public_key(cert_der: &[u8]) -> anyhow::Result<Vec<u8>> {
    let cert = x509_cert::Certificate::from_der(cert_der)?;
    let key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("subject public key BIT STRING is not aligned")?
        .to_owned();
    Ok(key)
}

struct Verifier {
    webpki: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    host: String,
    port: u16,
    trusted: Mutex<HashSet<String>>,
    known_path: Option<PathBuf>,
    prompt: Option<Arc<dyn RdpPrompt>>,
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
            tracing::warn!(?err, "failed to save RDP certificate fingerprint");
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
            RdpCertDecision::AcceptOnce
        } else if let Some(prompt) = &self.prompt {
            prompt.confirm_certificate(&RdpCertInfo {
                host: self.host.clone(),
                port: self.port,
                fingerprint: fp.clone(),
                reason: err.to_string(),
            })
        } else {
            RdpCertDecision::Reject
        };
        match decision {
            RdpCertDecision::Reject => Err(err),
            RdpCertDecision::AcceptOnce | RdpCertDecision::AcceptAndSave => {
                if decision == RdpCertDecision::AcceptAndSave {
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
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
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
