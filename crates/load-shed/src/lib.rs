//! load-shed — the primitives a process sheds load with, shared by every
//! process that refuses work under overload. Each process keeps its own
//! refusal order; only the parts below are common.
//!
//! - [`TokenBucket`] — a lazy-refill rate gate whose caller passes `now`.
//! - [`Ewma`] — the smoothing applied to a load reading.
//! - [`LoadSampler`] — the current-load read seam, with [`simulated`] for tests.
//! - [`retry_after`] — the `Retry-After` a refusal carries: a floor and a
//!   uniform jitter over a roll the caller supplies.
//!
//! Pure by construction: no clock, no timers, no I/O, no randomness of its
//! own. Time and rolls come from the caller, so a paused-clock test drives
//! these exactly as production does.

#![forbid(unsafe_code)]

mod ewma;
pub mod retry_after;
mod sampler;
mod token_bucket;

pub use ewma::Ewma;
pub use sampler::{clamp01, simulated, LoadSampler, SimulatedLoadControl, SimulatedLoadSampler};
pub use token_bucket::{at_ms, TokenBucket, ZERO_RATE_WAIT_SEC};
