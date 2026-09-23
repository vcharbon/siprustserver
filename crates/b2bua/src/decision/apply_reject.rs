//! `apply_reject` — translate a "reject" or "redirect" decision into call
//! state + the a-facing final it authors: the mark, the `service_ext` slices
//! exactly as [`apply_route`](super::apply_route::apply_route) seeds a route's,
//! then the final (a redirect's with its Contact list).

use std::collections::BTreeMap;

use call::helpers::{mark_decision, set_call_ext};
use call::{Call, DecisionKind, TerminationCause};
use sip_message::SipRequest;
use sip_txn::IdGen;

use super::schemas::{RedirectDecision, RejectDecision};
use crate::effects::HandlerResult;

/// Seed a decision's `service_ext` slices on `call` — a route's, a reject's or
/// a redirect's alike. A core-reserved key is not a service slice and no
/// service id may collide with it (ADR-0016): a decision cannot write it.
pub(super) fn seed_service_ext(
    mut call: Call,
    service_ext: BTreeMap<String, serde_json::Value>,
) -> Call {
    for (service_id, value) in service_ext {
        if crate::rules::relay::is_core_reserved_ext(&service_id) {
            continue;
        }
        call = set_call_ext(call, &service_id, Some(value));
    }
    call
}

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
    let call = mark_decision(call, now_ms, kind, leg_id, reject.label);
    let call = seed_service_ext(call, reject.service_ext);
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

/// Apply a redirect decision to `call` as [`apply_reject`] applies a reject:
/// mark it, seed its `service_ext`, then answer the a-leg with the 3xx and its
/// Contact list.
pub(crate) fn apply_redirect(
    call: Call,
    redirect: RedirectDecision,
    kind: DecisionKind,
    leg_id: Option<String>,
    a_invite: &SipRequest,
    id_gen: &IdGen,
    now_ms: i64,
) -> HandlerResult {
    let call = mark_decision(call, now_ms, kind, leg_id, redirect.label);
    let call = seed_service_ext(call, redirect.service_ext);
    crate::initial_invite::reject_call(
        call,
        a_invite,
        redirect.code,
        redirect.reason,
        redirect.update_headers.as_ref(),
        &redirect.contacts,
        id_gen,
        now_ms,
        TerminationCause::DecisionReject,
    )
}
