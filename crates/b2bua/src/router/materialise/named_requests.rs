//! What a call's record names of its non-INVITE requests: the transactions a copy
//! of a request meets, read off the record alone.

use call::{Call, RequestRecord};

use super::seeds::{invite_record, pending_via};

/// Whether `call` holds a non-INVITE request in flight on a dialog, other
/// than the keepalive: a relay or a request of its own awaiting its final, or
/// a request received and owed one — a transaction a copy serves, which the
/// party that began it may never repeat. The keepalive is left out: every
/// quiet dialog sends one, and taking each over is the stale-CSeq storm
/// ADR-0014 D1 avoids.
pub(in crate::router) fn names_request_in_flight(call: &Call) -> bool {
    call::helpers::every_record(call).any(|(_, _, r)| !invite_record(r) && !r.is_keepalive())
}

/// The non-INVITE record of `call` whose received half `branch` names: a
/// request a relay was made for, or one held for a later final. A copy of it
/// is no new request.
pub(in crate::router) fn received_record<'a>(
    call: &'a Call,
    branch: &str,
) -> Option<&'a RequestRecord> {
    if branch.is_empty() {
        return None;
    }
    call::helpers::every_record(call)
        .filter(|(_, _, r)| !r.is_own() && !invite_record(r))
        .find(|(_, _, r)| pending_via(r).is_some_and(|v| v.branch() == Some(branch)))
        .map(|(_, _, r)| r)
}

/// Whether `call` holds a final it sent on `branch` to a received `method`
/// request whose server transaction is Completed at `now_ms`: a copy of that
/// request draws the final again and is no new request (RFC 3261 §17.2.2).
pub(in crate::router) fn names_answered_branch(
    call: &Call,
    branch: &str,
    method: &str,
    now_ms: i64,
) -> bool {
    !branch.is_empty()
        && call
            .answered
            .iter()
            .any(|a| a.branch == branch && a.method == method && a.completed_at(now_ms))
}

/// `call` without the relay a copy of its originator's request on `branch`
/// supersedes: the relay's final returns to another node, so the copy is
/// relayed afresh and that relay settles nothing here.
pub(in crate::router) fn drop_superseded(mut call: Call, branch: &str) -> Call {
    let superseded = |r: &RequestRecord| {
        r.is_relay() && pending_via(r).is_some_and(|v| v.branch() == Some(branch))
    };
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        for d in leg.dialogs.iter_mut() {
            d.requests.retain(|r| !superseded(r));
        }
    }
    call
}
