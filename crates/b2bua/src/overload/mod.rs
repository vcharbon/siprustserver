//! Worker-side overload policy. Its one goal on the wire: **reject new
//! non-emergency calls when the worker is overloaded** — everything else
//! (in-dialog traffic, non-INVITE methods, emergency calls) is admitted.
//!
//! Two tiers shed, both emitting the same reject
//! ([`build_reject_new_call_503`]): the Tier-1 ingress brake
//! ([`crate::tier1_brake`]) fires at arrival time on a queue-depth threshold,
//! the Tier-3 admission gate here fires on the CPS bucket / panic-ELU
//! backstop. This module also publishes the `X-Overload` load signal the front
//! proxy's ELU-band AIMD consumes.
//!
//! - `reject` — [`build_reject_new_call_503`] + [`jittered_retry_after`] +
//!   [`StatelessRejectTagger`]: the single reject-new-call primitive both tiers
//!   send, and the request-derived identity the transactionless tier answers
//!   with.
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
//! The proxy's own self-gate is `sip_proxy::self_gate`.
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
mod reject;
mod sampler;
mod signal;

pub use admission::{AdmitDecision, AdmitReason};
pub use reject::{build_reject_new_call_503, jittered_retry_after, StatelessRejectTagger};
pub use sampler::{simulated, LoadSampler, SimulatedLoadControl, SimulatedLoadSampler};
pub use signal::{OverloadMetrics, OverloadSignal};

#[cfg(test)]
mod tests;
