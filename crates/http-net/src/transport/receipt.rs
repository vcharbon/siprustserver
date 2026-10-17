//! The instant the request being answered was received, visible to the service
//! for the duration of its answer.
//!
//! The real transport stamps it when the request head has been read and before
//! the body is, so a service measuring its own time from it counts the body's
//! transfer and its own queueing; the simulated fabric receives nothing off a
//! wire, so there it is unset.

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

tokio::task_local! {
    static RECEIPT: Receipt;
}

/// When a served request was received: the wall clock to state it, the
/// monotonic clock to measure from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The wall-clock reading at receipt.
    pub at: SystemTime,
    /// The monotonic reading at receipt.
    pub instant: Instant,
}

impl Receipt {
    /// A receipt stamped now.
    #[cfg_attr(not(feature = "real"), allow(dead_code))]
    pub(crate) fn now() -> Self {
        Self { at: SystemTime::now(), instant: Instant::now() }
    }

    /// The receipt of the request being answered, when the transport stamped one.
    pub fn current() -> Option<Self> {
        RECEIPT.try_with(|receipt| *receipt).ok()
    }

    /// The receipt as microseconds since the Unix epoch.
    pub fn unix_us(&self) -> i64 {
        self.at
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
            .unwrap_or_default()
    }

    /// Monotonic time since the receipt.
    pub fn elapsed(&self) -> Duration {
        self.instant.elapsed()
    }
}

/// Run `answer` with `receipt` as the current request's receipt.
#[cfg_attr(not(feature = "real"), allow(dead_code))]
pub(crate) async fn scope<F: Future>(receipt: Receipt, answer: F) -> F::Output {
    RECEIPT.scope(receipt, answer).await
}
