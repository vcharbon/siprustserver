//! `sip-proxy` — the stateless SIP front proxy + its load balancer (Rust port of
//! `src/sip-front-proxy/`, MIGRATION_STATUS slice 9).
//!
//! The proxy is a **stateless** RFC 3261 §16 proxy: it fans new dialogs across a
//! pool of B2BUA workers, pins in-dialog traffic to the chosen worker via a
//! signed Record-Route cookie, and tracks worker liveness with OPTIONS health
//! probes. It does **not** use the transaction layer's FSMs — CANCEL/ACK
//! correlation is a proxy-local `(Call-ID|CSeq#)` LRU ([`cancel_lru`]). It reuses
//! `sip-txn::IdGen` only for Via branch generation and `sip-clock::Clock` for
//! timestamps. See [ADR-0009](../../docs/adr/0009-front-proxy-rust-shape.md).
//!
//! ## Scope of this slice
//! - Ported: the proxy data path, the load balancer (HRW + signed cookie +
//!   routing matrix), the worker registry (static + simulated), OPTIONS health
//!   probing toward the B2BUA, and the metrics layer (counters + a Prometheus
//!   HTTP endpoint).
//! - Stubbed: [`self_gate`] is an always-admit stand-in; overload protection
//!   relies on OPTIONS-driven worker health/band + `sip-net`'s receive-buffer
//!   tail-drop for now.
//! - Deferred: the SIP registrar/REGISTER path, the per-worker AIMD rate-cap
//!   token bucket (band classification only here), and the kubernetes registry.

pub mod addr;
pub mod cancel_lru;
pub mod core;
pub mod headers;
pub mod health;
pub mod load_observer;
pub mod observability;
pub mod registry;
pub mod security;
pub mod self_gate;
pub mod strategy;
pub mod strategies;

pub use addr::ProxyAddr;
pub use core::{ProxyCore, ProxyCoreBuilder, ProxyCoreParts};
pub use observability::ProxyMetrics;
pub use strategies::{ForwardAllStrategy, LoadBalancerConfig, LoadBalancerStrategy};
pub use strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};
