//! The call admission set (ADR-0040): the limiter port (`port`, re-exported
//! here), the worker's one handle on the limiter ([`LimiterWorker`]) over the
//! worker-side tasks that talk to it ([`http`], [`bounded`], [`breaker`],
//! [`release_queue`], [`refresh_batch`], [`lease`]), and the call-side
//! recipes that apply its answers to a call ([`call`], `report`).

pub mod bounded;
pub mod breaker;
pub mod call;
pub mod http;
pub mod lease;
mod port;
pub mod refresh_batch;
pub mod release_queue;
pub(crate) mod report;
mod worker;

#[cfg(test)]
pub(crate) use port::testkit;
pub use port::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    NoopLimiter, RefreshAnswer, RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer,
    LOCAL_ADMIT_BUDGET,
};
pub use worker::LimiterWorker;
