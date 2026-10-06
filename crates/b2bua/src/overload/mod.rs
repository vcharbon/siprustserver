//! Worker-side overload signal: the inputs of the admission ladder's
//! panic-ELU and CPS bucket rungs ([`crate::admission`]), and the
//! `X-Overload` load signal the front proxy's ELU-band AIMD consumes.
//! In-dialog traffic and emergency calls pass both rungs.
//!
//! - `sampler` — the live tokio busy-ratio [`load_shed::LoadSampler`]; paused-clock tests
//!   inject readings through [`load_shed::simulated`] instead.
//! - `cpu_budget` — the process's cgroup CPU quota and affinity set, and the
//!   capacity the live sampler's busy ratio divides by.
//! - `signal` — [`OverloadSignal`]: EWMA state, the CPS bucket, the admit
//!   counters, and the `v=1; elu=…; gc=…; adm=…` header builder.
//! - `prometheus` — `/metrics` text exposition of the signal's inputs and
//!   admits.
//!
//! The consumer of the published header is the front proxy —
//! `sip_proxy::load_observer` parses exactly the `v=1` schema built here.
//! The proxy's own self-gate is `sip_proxy::self_gate`.
//!
//! The token bucket, EWMA, sampler seam and `Retry-After` policy are the
//! shared [`load_shed`] primitives.
//!
//! Clock contract: the token bucket refills on a `tokio::time::Instant`
//! timeline, so a `start_paused` test drives it with `tokio::time::advance`;
//! the live busy ratio measures REAL wall + busy time (monotonic, not
//! `tokio::time`) and reads ~0 under a paused idle runtime — paused tests
//! inject readings through [`load_shed::simulated`] instead. Signal state is
//! per-worker, in-memory, behind one `Mutex` (the read path is the OPTIONS-200
//! hot path but it is one cheap lock + a `String` format).

mod cpu_budget;
mod prometheus;
mod sampler;
mod signal;

pub use signal::{OverloadMetrics, OverloadSignal};

#[cfg(test)]
mod quota_tests;
#[cfg(test)]
mod tests;
