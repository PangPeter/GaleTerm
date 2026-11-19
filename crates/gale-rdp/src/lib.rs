//! RDP client for GaleTerm (Phase 3.5), built on IronRDP (Apache-2.0).

mod client;
mod error;
mod input;
mod tls;

pub use client::{
    FrameBuffer, RdpClientConfig, RdpInput, RdpOutput, RdpPrompt, RdpSession,
};
pub use error::{Error, Result};
