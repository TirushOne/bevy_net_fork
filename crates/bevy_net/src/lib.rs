//! The place where netcode and networking utilities for bevy live.

#[cfg(feature = "tls")]
pub use rustls;

#[cfg(feature = "quic")]
#[allow(missing_docs)]
pub mod quic;

#[cfg(feature = "tls")]
#[allow(missing_docs)]
pub mod crypto_utils;
