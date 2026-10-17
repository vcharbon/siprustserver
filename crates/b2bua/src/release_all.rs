//! The operator's release of every call a worker serves (ADR-0050 §10). Each
//! live call is sent one [`TOPIC`] event, run on the call's own queue under
//! its lock by the core `operator-release` rule:
//!
//! - an answered call ends with nothing sent: its legs are let go as they
//!   stand, its record closed under `operator_release`, its limiter holds
//!   released and its replica deleted by the ordinary release, so a later
//!   request for it draws `481`;
//! - a call in setup is aborted: `503` to the caller, CANCEL to a ringing
//!   callee;
//! - a call already ending is left to its teardown.
//!
//! No backend is consulted. Only an explicit request releases.

use std::sync::Arc;

use call::{Call, CallModelState};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::store::CallState;
use b2bua_sdk::event::CallEvent;

/// A worker's release-all, owned so the route that calls it outlives a borrow
/// of the worker ([`B2buaCore::release_all_handle`](crate::B2buaCore::release_all_handle)).
pub type ReleaseAllFn = Arc<dyn Fn() -> ReleasedAll + Send + Sync>;

/// The topic of the internal event that releases one call.
pub const TOPIC: &str = "operator-release";

/// The reason the closing CDR event of a released call carries.
pub const REASON: &str = "operator_release";

/// What a release-all reached, counted as it is requested: the answered calls
/// it releases and the calls in setup it aborts. A call that ends on its own
/// before its turn runs counts all the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReleasedAll {
    pub released: usize,
    pub aborted: usize,
}

/// Send every live, not yet ending call of `state` its release event on
/// `reentry`, the router's internal event channel.
pub(crate) fn release_all(state: &CallState, reentry: &UnboundedSender<CallEvent>) -> ReleasedAll {
    let mut reached = ReleasedAll::default();
    for call_ref in state.live_call_refs() {
        let Some(call) = state.peek(&call_ref) else { continue };
        if call.state != CallModelState::Active {
            continue;
        }
        if answered(&call) {
            reached.released += 1;
        } else {
            reached.aborted += 1;
        }
        let _ = reentry.send(CallEvent::InternalEvent {
            call_ref,
            topic: TOPIC.to_string(),
            outcome: String::new(),
            payload: serde_json::Value::Null,
            body: Vec::new(),
            incarnation: Some(call.incarnation().to_string()),
        });
    }
    tracing::info!(
        node = observe::node(),
        released = reached.released,
        aborted = reached.aborted,
        "operator release-all"
    );
    reached
}

/// An answered call is released with nothing sent; any other is aborted.
fn answered(call: &Call) -> bool {
    call::helpers::caller_answered(call)
}
