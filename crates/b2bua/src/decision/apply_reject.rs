//! `apply_reject` — translate a "reject" decision into call state + the
//! a-facing final: the mark, the `service_ext` slices exactly as
//! [`apply_route`](super::apply_route::apply_route) seeds a route's, then the
//! final the decision authors.

use call::helpers::{mark_decision, set_call_ext};
use call::{Call, DecisionKind, TerminationCause};
use sip_message::SipRequest;
use sip_txn::IdGen;

use super::schemas::RejectDecision;
use crate::effects::HandlerResult;

/// Apply a reject decision to `call`: mark it (`kind` says which decision
/// point returned it — the initial INVITE's, or a limiter failover's — and
/// `leg_id` the leg it answers), seed its `service_ext` (a core-reserved key
/// is skipped, ADR-0016), then answer the a-leg with the final.
pub(crate) fn apply_reject(
    call: Call,
    reject: RejectDecision,
    kind: DecisionKind,
    leg_id: Option<String>,
    a_invite: &SipRequest,
    id_gen: &IdGen,
    now_ms: i64,
) -> HandlerResult {
    let mut call = mark_decision(call, now_ms, kind, leg_id, reject.label);
    for (service_id, value) in reject.service_ext {
        if crate::rules::relay::is_core_reserved_ext(&service_id) {
            continue;
        }
        call = set_call_ext(call, &service_id, Some(value));
    }
    crate::initial_invite::reject_call(
        call,
        a_invite,
        reject.reject_code,
        reject.reject_reason,
        reject.update_headers.as_ref(),
        &[],
        id_gen,
        now_ms,
        TerminationCause::DecisionReject,
    )
}
