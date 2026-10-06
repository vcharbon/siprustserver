//! B2BUA runtime configuration. The type lives in the public Rule SDK
//! (`b2bua-sdk`, ADR-0016) so a service crate can read config without a
//! dependency on `b2bua`; this module re-exports it so in-tree `crate::config`
//! paths are unchanged.

pub use b2bua_sdk::config::*;
pub use b2bua_sdk::sdp_form::{AsWritten, SdpCrossing, SdpForm, SdpFormPolicy};
