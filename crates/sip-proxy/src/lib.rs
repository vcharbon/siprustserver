//! `sip-proxy` — the stateless SIP front proxy + its load balancer.
//!
//! The proxy is a **stateless** RFC 3261 §16 proxy: it fans new dialogs across a
//! pool of B2BUA workers, pins in-dialog traffic to the chosen worker via a
//! signed Record-Route cookie, and tracks worker liveness with OPTIONS health
//! probes. It does **not** use the transaction layer's FSMs: the branch it
//! pushes on its Via is a function of the message (RFC 3261 §16.11,
//! `branch`), and the CANCEL/ACK hop is a proxy-local
//! `(Call-ID|From-tag|CSeq#)` LRU ([`cancel_lru`]). It reuses `sip-txn::IdGen`
//! for the To-tag of its own finals and `sip-clock::Clock` for timestamps.
//! See [ADR-0009](../../docs/adr/0009-front-proxy-rust-shape.md).
//!
//! ## Scope
//! - Included: the proxy data path ([`core`]), the load balancer (HRW + signed
//!   cookie + routing matrix, [`strategies`]), the worker registry (static +
//!   simulated), OPTIONS health probing toward the B2BUA, the metrics layer
//!   (counters + a Prometheus HTTP endpoint), and [`self_gate`] — the ELU/CPS
//!   admission gate ([`self_gate::EluCpsGate`]: EWMA-smoothed intake pressure
//!   (packet age at dequeue) + per-class CPS token bucket) shedding external new-dialog
//!   non-emergency INVITEs under self-overload, with the always-admit
//!   [`self_gate::AlwaysAdmitGate`] as the no-protection default, and the
//!   per-call trace tier ([`trace`], ADR-0026: independent sampling on the
//!   initial INVITE, correlation by `Call-ID` only, nothing added to the wire).
//! - Out of scope: the SIP registrar/REGISTER path, the per-worker AIMD
//!   rate-cap token bucket (band classification only here), and the
//!   kubernetes registry.

pub mod addr;
mod branch;
pub mod cancel_lru;
pub mod core;
pub mod face;
pub mod headers;
pub mod health;
pub mod liveness;
pub mod load_observer;
pub mod observability;
pub mod registry;
pub mod resolver;
pub mod security;
pub mod self_gate;
pub mod strategies;
pub mod strategy;
pub mod trace;

pub use addr::ProxyAddr;
pub use core::{ExternalFaceParts, ProxyCore, ProxyCoreBuilder};
pub use face::{FaceCidrs, Ipv4Cidr};
pub use observability::ProxyMetrics;
pub use strategies::{ForwardAllStrategy, LoadBalancerConfig, LoadBalancerStrategy};
pub use strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};
pub use trace::ProxyTraces;
