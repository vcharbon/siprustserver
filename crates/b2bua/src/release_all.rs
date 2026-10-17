//! The operator's release of every call a worker serves (ADR-0050 §10). Each
//! live call is sent one [`TOPIC`] event, run on the call's own queue under
//! its lock by the core `operator-release` rule (`rules::operator_release`):
//!
//! - an answered call ends with no BYE: the ACK a 2xx is still owed is sent, a
//!   leg still ringing is CANCELled, every other leg let go as it stands; its
//!   record is closed under `operator_release`, its limiter holds released and
//!   its replica deleted by the ordinary release, so a later request for it
//!   draws `481`;
//! - a call in setup is aborted: `503` to the caller, CANCEL to a ringing
//!   callee;
//! - a call already ending is left to its teardown.
//!
//! No backend is consulted. Only an explicit request releases. A takeover
//! copy is released as the worker's own calls are, and defers the call's end
//! to its primary as every ending copy does (ADR-0020 X3).

use std::sync::Arc;

use serde::Serialize;

use crate::router::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// A worker's release-all, owned so the route that calls it outlives a borrow
/// of the worker ([`B2buaCore::release_all_handle`](crate::B2buaCore::release_all_handle)).
pub type ReleaseAllFn = Arc<dyn Fn() -> ReleasedAll + Send + Sync>;

/// The topic of the internal event that releases one call.
pub const TOPIC: &str = "operator-release";

/// The reason the closing CDR event of a released call carries.
pub const REASON: &str = "operator_release";

/// What a release-all reached, counted as it is requested: the answered calls
/// it releases and the calls in setup it aborts. A call whose state moves
/// before its turn runs is counted as it was; its turn reads it as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReleasedAll {
    pub released: usize,
    pub aborted: usize,
}

/// Send every live, not yet ending call of `ctx` its release event on the
/// router's internal event channel, and count what it reached
/// (`b2bua_operator_released_total`).
pub(crate) fn release_all(ctx: &RouterCtx) -> ReleasedAll {
    let mut reached = ReleasedAll::default();
    for (call_ref, incarnation, answered) in ctx.state.active_call_answers() {
        if answered {
            reached.released += 1;
        } else {
            reached.aborted += 1;
        }
        let _ = ctx.reentry_tx.send(CallEvent::InternalEvent {
            call_ref,
            topic: TOPIC.to_string(),
            outcome: String::new(),
            payload: serde_json::Value::Null,
            body: Vec::new(),
            incarnation: Some(incarnation),
        });
    }
    ctx.metrics.count_operator_release(reached);
    tracing::info!(
        node = observe::node(),
        released = reached.released,
        aborted = reached.aborted,
        "operator release-all"
    );
    reached
}
