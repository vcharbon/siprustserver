//! The spawned task around one placed call.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;

use crate::outcome::OutcomeSink;
use crate::report::Reporter;
use crate::scenarios::ScenarioId;

/// The spawned task of one placed call: `call` held once, behind its panic
/// boundary. A panic outside the scenario's own boundary (checks, sample
/// rendering, the bookkeeping) still closes the call's in-flight count and is
/// counted as a generator failure: the call wrote no record.
pub(crate) fn call_task<F: Future<Output = ()>>(
    call: F,
    id: ScenarioId,
    reporter: Arc<Reporter>,
    outcomes: Option<Arc<OutcomeSink>>,
) -> impl Future<Output = ()> {
    AssertUnwindSafe(call).catch_unwind().map(move |r| {
        if r.is_err() {
            reporter.dec_inflight();
            if let Some(sink) = outcomes {
                sink.generator_failure(id, "panic");
            }
        }
    })
}
