//! The route guard of a BGP-announced front proxy's node (ADR-0056): the
//! fast path that takes the node out of its routers' ECMP when the proxy on
//! it cannot carry traffic, beside the readiness path that MetalLB follows.
//!
//! It reads the proxy's `/fitz` every `ROUTE_GUARD_POLL_MS` over loopback and
//! owns one nftables table that drops the node's BFD and BGP toward its
//! routers unless they are in a set whose entries time out
//! `ROUTE_GUARD_DEAD_MS` after each refresh. While the proxy reads fit the
//! guard refreshes the set; a second unfit answer since the last fit one
//! empties it; a proxy, or a guard, that stops refreshing lets it run out.
//! Either way the routers miss the node's BFD for their detection time and
//! withdraw every route through it, as for a dead node. A withdrawn node is
//! let out again once the proxy read fit for `ROUTE_GUARD_HOLD_MS`; its BGP
//! sessions then re-establish. The guard's `/readyz` is Ready exactly while
//! the node is let out. Nothing in the path waits on the Kubernetes API.

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

pub mod config;
pub mod fitness;
pub mod metrics;
pub mod nft;
pub mod rules;
pub mod run;
pub mod switch;
