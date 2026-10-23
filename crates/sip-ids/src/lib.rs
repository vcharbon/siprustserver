//! sip-ids — the derivation of SIP identifiers: From/To tags, Via branches
//! and the starts of CSeq and RSeq sequences (RFC 3261 §8.1.1, §19.3; RFC 3262
//! §7.1), each drawn as HMAC-SHA256 under a secret key, so an off-path host
//! cannot guess the next one and two keys give unrelated streams.
//!
//! - [`IdKey`] — the keyed PRF; its counter stream is HMAC(key, n).
//! - [`TurnIds`] — the draws of one turn: HMAC(key, turn ‖ n).
//! - [`render`] — a draw rendered as a tag, a branch or a sequence start.
//!
//! Pure: the key enters as a value. The entropy that keys a process, and the
//! counter its draws share, are the caller's.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::iter_over_hash_type,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod key;
pub mod render;
mod turn;

pub use key::IdKey;
pub use render::MAGIC_COOKIE;
pub use turn::TurnIds;
