//! amqp-connect — opening an AMQP 0-9-1 connection on the running tokio
//! runtime. The TCP connect and the TLS handshake run on the caller's task,
//! under the caller's deadline ([`connect_stream`], [`connect_uri`]); the
//! client is then handed the ready stream ([`open_over`]). Its IO loop never
//! connects itself, so a failed connect never reaches the client and no tokio
//! timer runs on the client's IO thread.

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

mod handover;
mod stream;
mod uri;

pub use handover::open_over;
pub use stream::{connect_stream, BrokerStream, Connected};
pub use uri::connect_uri;
