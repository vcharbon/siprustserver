//! Per-`(LB, worker)` AIMD admission control fed by the `X-Overload` payload
//! workers stamp on OPTIONS replies (ADR-0009 X6).
//!
//! Each LB keeps one [`WorkerLoadObserver`] holding one AIMD bucket per worker:
//!
//!   - [`apply_payload`](WorkerLoadObserver::apply_payload) takes each payload the
//!     OPTIONS health probe hands in and runs one AIMD step: additive increase on
//!     cool workers, multiplicative decrease on hot ones, and a pin-to-floor when
//!     `elu > elu_critical`.
//!   - Band boundaries carry hysteresis, so a worker oscillating around a
//!     threshold does not flap.
//!   - A cooldown follows every decrease, so the worker sheds in-flight load
//!     before increases resume.
//!   - [`try_consume_for`](WorkerLoadObserver::try_consume_for) serves the
//!     new-dialog admit path and [`band_for`](WorkerLoadObserver::band_for) the
//!     `above_critical` filter.
//!   - A payload older than `payload_stale_ms` triggers one conservative
//!     decrease on the next [`sweep_stale`](WorkerLoadObserver::sweep_stale) tick.
//!
//! Every method that needs the current time takes `now_ms: i64` (epoch ms, the
//! units of [`sip_clock::Clock::now_ms`]). Nothing here arms a `tokio::time`
//! timer: the bucket refill, the cooldown and the stale window are `now_ms`
//! arithmetic.

mod band;
mod config;
mod observer;
mod payload;
mod sweep;
#[cfg(test)]
mod tests;

pub use band::EluBand;
pub use config::LoadObserverConfig;
#[cfg(test)]
pub use observer::AimdSnapshot;
pub use observer::{AimdAction, WorkerLoadObserver};
pub use payload::{parse_x_overload_header, OverloadPayload, X_OVERLOAD};
pub use sweep::sweep_alive;
