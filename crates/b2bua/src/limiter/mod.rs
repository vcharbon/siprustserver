//! The call admission set (ADR-0040): the limiter port (`port`, re-exported
//! here), the worker's one handle on the limiter ([`LimiterWorker`]) over the
//! worker-side tasks that talk to it ([`http`], [`bounded`], [`breaker`],
//! [`release_queue`], [`refresh_batch`], [`lease`]), the call-side recipes
//! that apply its answers to a call (the core's `call` and `report`,
//! re-exported here), and the refresh's turns (`refresh_turn`).

pub mod bounded;
pub mod breaker;
pub mod http;
pub mod lease;
mod port;
pub mod refresh_batch;
pub(crate) mod refresh_turn;
pub mod release_queue;
mod worker;

pub use b2bua_core::limiter::*;

#[cfg(test)]
pub(crate) use port::testkit;
pub use port::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    NoopLimiter, RefreshAnswer, RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer,
    LOCAL_ADMIT_BUDGET,
};
pub use worker::LimiterWorker;
