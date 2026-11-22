// ADR-0051 §3
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

pub mod binds;
pub mod config;
pub mod debug_server;
pub mod discovery;
pub mod external_face;
pub mod health_sampler;
pub mod intake_ramp;
pub mod listening;
pub mod metrics_body;
pub mod readiness;
pub mod runtime;
pub mod shards;
pub mod signal;
pub mod sweeps;
