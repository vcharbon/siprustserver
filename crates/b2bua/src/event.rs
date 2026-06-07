//! The unified call event. The type moved to the public Rule SDK (`b2bua-sdk`,
//! ADR-0016 slice 6) so a service crate's rules can match on it without a
//! dependency on `b2bua`; this module re-exports it so in-tree `crate::event`
//! paths are unchanged.

pub use b2bua_sdk::event::*;
