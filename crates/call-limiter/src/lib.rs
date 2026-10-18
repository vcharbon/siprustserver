//! `call-limiter` — a concurrent-call limiter keyed by the call, served as a
//! dedicated HTTP process and shared cluster-wide.
//!
//! This crate is b2bua-agnostic. It carries:
//! - [`CallStore`] — the keyed core: per call the entries it holds, the change
//!   number of its last admit and a lease, per id the live count. One `admit`
//!   replaces a call's whole set atomically, checked net of the set it already
//!   holds and refused when older than the set held; `release` is
//!   idempotent by call; `refresh` extends the lease, or re-creates a set the
//!   store no longer holds; a lapsed lease drops the set.
//! - [`LimiterServer`] — an [`http_net::HttpService`] routing `/v1/*` +
//!   `/metrics` + `/healthz` onto the core (`/v1/health` reads the store).
//! - [`LimiterMetrics`] — global counters + gauges (no per-id labels).
//!
//! The [`wire`] DTOs of the HTTP API (every request names its call) and the
//! lease bounds live in `call-limiter-wire`, which the HTTP client in `b2bua`
//! depends on alone; the client and its fail-open policy live in `b2bua`.

mod catalogue;
mod metrics;
mod server;
mod store;

pub use call_limiter_wire as wire;
pub use call_limiter_wire::{DEFAULT_LEASE_SEC, MAX_LEASE_SEC};
pub use catalogue::CATALOGUE;
pub use metrics::LimiterMetrics;
pub use server::LimiterServer;
pub use store::{AdmitResult, CallStore, LimiterConfig, RefreshResult, Refreshed, StoreStats};
