//! The front proxy runner's library half: its boot configuration, its worker
//! pool, its health sampler, its intake shed, its readiness, the state its
//! recv shards share, and what its `/metrics` body holds.

// The panic set at deny outside test code (ADR-0051 §3).
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
pub mod discovery;
pub mod health_sampler;
pub mod metrics_body;
pub mod readiness;
pub mod runtime;
pub mod shards;
pub mod signal;
pub mod sweeps;
