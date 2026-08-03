//! Per-call tracing for the front proxy (ADR-0026).
//!
//! The proxy samples **independently** of the workers: no span context ever
//! rides the SIP wire and no header is added, stripped or propagated. The two
//! processes' spans are correlated by the `sip.call_id` attribute alone.
//!
//! One concern per submodule: [`registry`] owns the admission gate and the live
//! `Call-ID -> root span` map (bounded by the active-trace cap, closed on an
//! observed BYE final or on TTL); [`emit`] owns the guarded vocabulary — the
//! datagrams in and out and the routing facts a traced call records.
//!
//! **Explicit-guard discipline.** Every emission goes through
//! [`registry::ProxyTraces::with_span`], whose first act is one relaxed load of
//! the "anything sampled?" flag. With nothing sampled the per-packet path is a
//! single predicted branch: no lookup, no formatting, no allocation.

pub mod emit;
pub mod registry;

pub use registry::{ProxyTraces, SPAN_IDLE_TTL_MS};
