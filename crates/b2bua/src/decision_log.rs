//! Feeding the decision log ([`call::helpers::mark_decision`]) from the async
//! folds: the failure, release and refer consults come back as internal events
//! and the mark is made once, where the fold lands, before any rule reads it —
//! whichever rule then applies the fold (a service's or the core's), every
//! message and event it emits is stamped with the decision's ordinal.
//!
//! A mark records a decision the decision layer returned AND the stack
//! applied. A fold the callout resolved on its own — an unanswered consult, a
//! limiter-refused reroute, the terminal limiter final — carries
//! [`STACK_ORIGIN`] and marks nothing; a fold landing on a call already going
//! away applies nothing and marks nothing.

use call::helpers::mark_decision;
use call::{Call, CallModelState, DecisionKind};

use crate::event::CallEvent;
use crate::rules::defaults::parse_label;

/// The payload key a callout sets (`true`) on a fold it resolved on the
/// stack's own account, with no decision behind it.
pub(crate) const STACK_ORIGIN: &str = "stack_authored";

/// The `call-failure-result` / `call-release-result` / `refer-http-result`
/// topics and the outcomes that are decisions.
pub(crate) const FAILURE_TOPIC: &str = "call-failure-result";
pub(crate) const RELEASE_TOPIC: &str = "call-release-result";
pub(crate) const REFER_TOPIC: &str = "refer-http-result";

/// Whether a fold's payload carries [`STACK_ORIGIN`]: the callout resolved
/// it on the stack's own account, no decision behind it.
pub(crate) fn stack_authored(payload: &serde_json::Value) -> bool {
    payload.get(STACK_ORIGIN).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Mark the decision a fold carries, if it is one the stack applies: the kind
/// from the topic and outcome, the label from the payload, the leg from the
/// payload's failed leg (a failover) or the transfer's referrer (a refer).
pub(crate) fn fold_decided(call: Call, event: &CallEvent, now_ms: i64) -> Call {
    let CallEvent::InternalEvent { topic, outcome, payload, .. } = event else {
        return call;
    };
    if stack_authored(payload)
        || matches!(call.state, CallModelState::Terminating | CallModelState::Terminated)
    {
        return call;
    }
    let failed_leg = || {
        payload
            .get("failed_leg_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let referrer = || call.transfer.as_ref().map(|t| t.referrer_leg_id.clone());
    let (kind, leg_id) = match (topic.as_str(), outcome.as_str()) {
        (FAILURE_TOPIC, "failover") => (DecisionKind::FailoverRoute, failed_leg()),
        (FAILURE_TOPIC, "terminate") => (DecisionKind::FailoverTerminate, failed_leg()),
        (FAILURE_TOPIC, "reject") => (DecisionKind::FailoverReject, failed_leg()),
        (FAILURE_TOPIC, "redirect") => (DecisionKind::FailoverRedirect, failed_leg()),
        (RELEASE_TOPIC, "release") => (DecisionKind::Release, None),
        (RELEASE_TOPIC, "reroute") => (DecisionKind::ReleaseRoute, None),
        // A transfer whose slice is gone has nothing to apply the answer to.
        (REFER_TOPIC, "allow") if call.transfer.is_some() => {
            (DecisionKind::TransferAllow, referrer())
        }
        (REFER_TOPIC, "reject") if call.transfer.is_some() => {
            (DecisionKind::TransferReject, referrer())
        }
        _ => return call,
    };
    mark_decision(call, now_ms, kind, leg_id, parse_label(payload))
}
