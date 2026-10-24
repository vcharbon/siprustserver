//! `b2bua-decision` — the call-decision seam: what a decision backend
//! implements and what it is handed. The decision DTOs are the core's
//! (`b2bua-core`, ADR-0052), re-exported here; [`CallDecisionEngine`] is the
//! port the router consults; [`AdaptationHttpPort`] is the backend a
//! service's async HTTP callback is sent to.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod adaptation_port;
mod engine;

pub use adaptation_port::AdaptationHttpPort;
pub use b2bua_core::decision::*;
pub use engine::CallDecisionEngine;
