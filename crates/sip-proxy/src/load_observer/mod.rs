//! Per-`(LB, worker)` AIMD admission control fed by the `X-Overload` payload
//! workers stamp on OPTIONS replies.
//!
//! Each LB keeps one [`WorkerLoadObserver`] holding one AIMD bucket per worker:
//!
//!   - Eats `X-Overload` payloads handed in by the OPTIONS health probe
//!     ([`apply_payload`](WorkerLoadObserver::apply_payload)).
//!   - Runs AIMD on each payload: additive increase on cool workers,
//!     multiplicative decrease on hot ones, and a pin-to-floor when
//!     `elu > elu_critical`.
//!   - Hysteresis on band boundaries so a worker oscillating around a threshold
//!     does not flap.
//!   - A cooldown after every decrease so the worker has time to shed in-flight
//!     load before increases resume.
//!   - Exposes [`try_consume_for`](WorkerLoadObserver::try_consume_for) for the
//!     new-dialog admit path and [`band_for`](WorkerLoadObserver::band_for) for
//!     the `above_critical` filter.
//!   - Stale payloads (older than `payload_stale_ms`) trigger one conservative
//!     decrease on the next [`sweep_stale`](WorkerLoadObserver::sweep_stale) tick.
//!
//! Module map: [`payload`] — the `X-Overload` value codec; [`band`] — ELU band
//! classification with hysteresis; [`config`] — tunables + the band-config
//! validator the runner preflight calls; [`observer`] — the per-worker AIMD
//! state machine and its admit/sweep/snapshot API.
//!
//! ## Clock — explicit `now_ms`, NOT `tokio::time`
//!
//! Every method that needs the current time takes `now_ms: i64` explicitly
//! (epoch-ms, the same units [`sip_clock::Clock::now_ms`] hands out), so the
//! AIMD ladder is a pure state machine the unit tests drive with literal
//! timestamps. The production caller (the OPTIONS health probe) passes
//! `clock.now_ms()`; under a paused runtime that advances in lockstep with
//! `tokio::time::advance`, so there is no separate clock to keep in sync
//! (CLAUDE.md). Nothing here arms a `tokio::time` timer — the bucket refill,
//! cooldown and stale windows are all `now_ms` arithmetic. This is deliberately
//! simpler than the `self_gate` `tokio::time::Instant` bucket: that gate has no
//! second party and refills against ambient time; this observer is fed a
//! timestamp by whoever drives the probe/sweep cadence.

mod band;
mod config;
mod observer;
mod payload;
#[cfg(test)]
mod tests;

pub use band::EluBand;
pub use config::LoadObserverConfig;
pub use observer::{AimdAction, AimdSnapshot, WorkerLoadObserver};
pub use payload::{parse_x_overload_header, OverloadPayload};
