//! TLS upgrade for the RDP security handshake (accepts any peer cert for MVP).

use std::io::Write as _;
use std::net::TcpStream;
use std::sync::Arc;

use anyhow::Context as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use x509_cert::der::Decode as _;

pub(crate) type TlsStream = rustls::StreamOwned<ClientConnection, TcpStream>;

pub(crate) fn upgrade(stream: TcpStream, server_name: &str) -> anyhow::Result<(TlsStream, Vec<u8>)> {
    // sspi/reqwest may also pull aws-lc-rs; pick one process-wide provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
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

#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}
