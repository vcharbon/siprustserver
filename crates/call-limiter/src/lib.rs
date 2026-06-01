//! `call-limiter` — a sliding-window concurrent-call limiter, served as a
//! dedicated stateless HTTP process and shared cluster-wide.
//!
//! This crate is b2bua-agnostic. It carries:
//! - [`WindowStore`] — the windowed counter core. A faithful port of the TS
//!   in-memory limiter (`CallLimiter.memory.ts`): N active windows summed,
//!   per-key TTL, whole-store sweep-on-access. The per-op atomics match the
//!   Redis Lua scripts (`CallLimiter.redis.ts`): admit = sum-then-incr, refresh
//!   = incr-current-before-decr-origin (never undercounts), release = decrement
//!   floored at 0.
//! - The [`wire`] DTOs for the **batched, transactional** HTTP API: one `admit`
//!   carries every limiter entry for a call and increments **all or none**.
//! - [`LimiterServer`] — an [`http_net::HttpService`] routing `/v1/*` +
//!   `/metrics` + `/healthz` onto the core.
//! - [`LimiterMetrics`] — global counters + gauges (no per-id labels).
//!
//! The HTTP client (and the fail-open policy) live in `b2bua`.

mod metrics;
mod server;
pub mod wire;
mod window;

pub use metrics::LimiterMetrics;
pub use server::LimiterServer;
pub use window::{AdmitResult, LimiterConfig, WindowStore};
