//! `call-limiter` — a concurrent-call limiter keyed by the call, served as a
//! dedicated HTTP process and shared cluster-wide.
//!
//! This crate is b2bua-agnostic. It carries:
//! - [`CallStore`] — the keyed core: per call the multiset of ids it holds and
//!   a lease, per id the live count. One `admit` replaces a call's whole set
//!   atomically, checked net of the set it already holds; `release` is
//!   idempotent by call; `refresh` extends the lease, or re-creates a set the
//!   store no longer holds; a lapsed lease drops the set.
//! - The [`wire`] DTOs of the HTTP API: every request names its call.
//! - [`LimiterServer`] — an [`http_net::HttpService`] routing `/v1/*` +
//!   `/metrics` + `/healthz` onto the core (`/v1/health` reads the store).
//! - [`LimiterMetrics`] — global counters + gauges (no per-id labels).
//!
//! The HTTP client (and the fail-open policy) live in `b2bua`.

mod metrics;
mod server;
mod store;
pub mod wire;

pub use metrics::LimiterMetrics;
pub use server::LimiterServer;
pub use store::{
    AdmitResult, CallStore, LimiterConfig, RefreshResult, StoreStats, DEFAULT_LEASE_SEC,
    MAX_LEASE_SEC,
};
