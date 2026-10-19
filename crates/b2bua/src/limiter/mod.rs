//! The call admission set (ADR-0040): the limiter port (`port`, re-exported
//! here), the worker's one handle on the limiter ([`LimiterWorker`]) over the
//! worker-side tasks that talk to it ([`http`], [`bounded`], [`breaker`],
//! [`release_queue`], [`refresh_batch`], [`lease`]), the call-side recipes
//! that apply its answers to a call ([`call`], [`report`]), and the refresh's
//! turns ([`refresh_turn`]).

use std::time::Duration;

pub mod bounded;
pub mod breaker;
pub mod call;
pub mod http;
pub mod lease;
mod port;
pub mod refresh_batch;
pub(crate) mod refresh_turn;
pub mod release_queue;
pub mod report;
mod worker;

/// How long past the limiter's admit budget the worker waits for any admit
/// before it answers `unavailable`: a client's own timeout fires first and is
/// counted. Well inside the answer deadline's margin (ADR-0039).
pub const ADMIT_SLACK: Duration = Duration::from_millis(100);

#[cfg(test)]
pub(crate) use port::testkit;
pub use port::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    NoopLimiter, RefreshAnswer, RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer,
    LOCAL_ADMIT_BUDGET,
};
pub use worker::LimiterWorker;
