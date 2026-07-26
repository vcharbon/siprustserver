//! Worker-side overload surface: the `X-Overload` publish signal the front
//! proxy's ELU-band AIMD consumes, plus the Tier-3 admission gate that sheds
//! new-dialog INVITEs with a stateless 503 when the worker itself saturates.
//!
//! - `sampler` — the [`LoadSampler`] read seam: the live tokio busy-ratio
//!   sampler and the injectable [`simulated`] pair for paused-clock tests.
//! - `ewma` — the smoothing primitive behind the published readings.
//! - `bucket` — the hard CPS token bucket (rides `tokio::time::Instant`).
//! - `admission` — the gate's verdict types ([`AdmitDecision`],
//!   [`AdmitReason`]), tunables and seed defaults.
//! - `signal` — [`OverloadSignal`]: EWMA state + admit/reject counters, the
//!   `v=1; elu=…; gc=…; adm=…` header builder, and the
//!   [`should_admit`](OverloadSignal::should_admit) gate.
//! - `prometheus` — `/metrics` text exposition of the gate's inputs + decisions.
//!
//! The consumer of the published header is the front proxy —
//! `sip_proxy::load_observer` parses exactly the `v=1` schema built here.
//! The Tier-1 ingress brake does NOT live here — see [`crate::tier1_brake`];
//! the proxy's own self-gate is `sip_proxy::self_gate`.
//!
//! Clock contract: the token bucket refills off `tokio::time::Instant`, so a
//! `start_paused` test drives it with `tokio::time::advance`; the live busy
//! ratio measures REAL wall + busy time (monotonic, not `tokio::time`) and
//! reads ~0 under a paused idle runtime — paused tests inject readings through
//! [`simulated`] instead. Signal state is per-worker, in-memory, behind one
//! `Mutex` (the read path is the OPTIONS-200 hot path but it is one cheap lock
//! + a `String` format).

mod admission;
mod bucket;
mod ewma;
mod prometheus;
mod sampler;
mod signal;

pub use admission::{AdmitDecision, AdmitReason};
pub use sampler::{simulated, LoadSampler, SimulatedLoadControl, SimulatedLoadSampler};
pub use signal::{OverloadMetrics, OverloadSignal};

#[cfg(test)]
mod tests;
