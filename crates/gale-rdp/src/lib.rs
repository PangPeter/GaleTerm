//! RDP client for GaleTerm (Phase 3.5), built on IronRDP (Apache-2.0).

mod client;
mod cliprdr;
mod error;
mod input;
mod tls;

pub use client::{
    FrameBuffer, RdpCertDecision, RdpCertInfo, RdpClientConfig, RdpInput, RdpOutput, RdpPrompt,
    RdpSession,
};
pub use error::{Error, Result};
pub use tls::default_known_certs_path;
