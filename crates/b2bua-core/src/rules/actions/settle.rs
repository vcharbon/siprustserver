//! The one settle entry (ADR-0055 §2): how a call answers the requests it
//! holds, or owes a final for, as a leg ends or the call is released.
//! [`settle`] is called at these triggers and nowhere else:
//!
//! | trigger | caller | legs | cause |
//! |---|---|---|---|
//! | the termination begins | `begin_termination` | every | [`Cause::TerminationBegins`] |
//! | the caller's INVITE carries its final at termination | `begin_termination` | the caller's | [`Cause::LegEnd`] |
//! | a ringing leg's CANCEL leaves at termination | `begin_termination` | that leg | [`Cause::LegEnd`] |
//! | a rule destroys a leg (`DestroyLeg`) | `destroy_leg` | that leg | [`Cause::LegEnd`] |
//! | a rule ends a leg (`TerminateLeg`) | `terminate_leg` | that leg | [`Cause::LegEnd`] |
//! | the call is released at once (`TerminateCall`) | `dispatch` | every | [`Cause::LegEnd`] |
//! | the unanswered caller's 503 leaves, at termination (ADR-0022) or under the possibly-confirmed hold (ADR-0055 §4) | `answer_unanswered_caller` | the caller's | [`Cause::LegEnd`] |
//!
//! A Confirmed leg BYEd at termination is not settled: its relays wait for
//! their real finals. A discharge never settles (ADR-0022 X5): it sends
//! nothing. A service's own answer to one named request (`AnswerHeld`,
//! `RejectPendingReinvite`) and the platform's answer at a held request's
//! deadline are not settlements: each answers that request through its own
//! action.

use call::helpers::{find_leg, update_leg, RelayPeer};
use call::{Call, CallModelState, Dialog, DialogId, RequestRecord};

use crate::effects::HandlerEffects;
use crate::rules::method_table::row_of;

use super::request_record::{answer_received, arm_held_answer};
use super::ActionExecutor;

/// The final a request still held when its dialog ends draws (RFC 3261
/// §15.1.2).
const ENDED_HELD: (u16, &str) = (487, "Request Terminated");

/// The legs a settlement covers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Legs<'a> {
    /// The caller's, then every b-leg's, in the record's order.
    Every,
    /// This leg's alone.
    One(&'a str),
}

/// What a settlement answers.
pub(crate) enum Cause<'e, 'x> {
    /// The call's termination begins: every relayed INVITE still pending is
    /// answered 487 and its target CANCELled (§15.1.2, §9.1) through the
    /// turn's executor, then every request a service held is answered 487.
    /// The relays of other methods wait for their finals.
    TerminationBegins(&'e mut ActionExecutor<'x>),
    /// The legs end: each relay toward them draws the method table's owed
    /// final for its originator (§8.2.6), each request held on them 487, and
    /// each leaves the record, so a late final from the target is never a
    /// second final (§17.2.1). This stack's own requests stay until their
    /// final or their timeout; a relayed INVITE a retired dialog still holds
    /// ends with the leg, unanswered here.
    LegEnd,
}

/// Which records of a leg a settlement answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owes {
    /// The requests held for a service.
    Held,
    /// The held requests and the non-INVITE relays toward the leg.
    AtLegEnd,
}

/// Settle `legs` of `call` for `cause` at `now_ms`, the answers on `fx`.
/// Where a held request left the record, the held-answer timer stands at the
/// earliest deadline still held, unless the call is already terminated.
pub(crate) fn settle(
    call: &mut Call,
    fx: &mut HandlerEffects,
    legs: Legs<'_>,
    cause: Cause<'_, '_>,
    now_ms: i64,
) {
    let ids: Vec<String> = match legs {
        Legs::Every => std::iter::once(&call.a_leg)
            .chain(call.b_legs.iter())
            .map(|l| l.leg_id.clone())
            .collect(),
        Legs::One(id) => vec![id.to_string()],
    };
    let owes = match cause {
        Cause::TerminationBegins(exec) => {
            for (leg_id, cseq) in pending_invites(call, &ids) {
                exec.reject_pending_reinvite(call, fx, &leg_id, cseq, 487, "Request Terminated");
            }
            Owes::Held
        }
        Cause::LegEnd => Owes::AtLegEnd,
    };
    let mut held_left = false;
    for id in &ids {
        held_left |= answer_owed(call, fx, id, owes);
    }
    if held_left && call.state != CallModelState::Terminated {
        arm_held_answer(call, fx, now_ms);
    }
}

/// The relayed INVITEs still pending on a dialog of the legs `ids`, not yet
/// CANCELled, as `(leg, outbound CSeq)`.
fn pending_invites(call: &Call, ids: &[String]) -> Vec<(String, i64)> {
    ids.iter()
        .filter_map(|id| find_leg(call, id))
        .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
        .flat_map(|(leg, d)| {
            call::helpers::invite_relay(d)
                .filter(|p| !p.cancelled)
                .map(move |p| (leg.leg_id.clone(), p.outbound_cseq))
        })
        .collect()
}

/// Answer every record of `leg_id` that `owes` selects, the live dialogs'
/// first, then the retired ones', each leaving the record. Returns whether a
/// held request left.
fn answer_owed(call: &mut Call, fx: &mut HandlerEffects, leg_id: &str, owes: Owes) -> bool {
    let Some(leg) = find_leg(call, leg_id) else { return false };
    let owed = |r: &RequestRecord| match owes {
        Owes::Held => r.is_held(),
        Owes::AtLegEnd => (r.is_relay() || r.is_held()) && !r.is_invite(),
    };
    let (retired, live): (Vec<&Dialog>, Vec<&Dialog>) =
        leg.dialogs.iter().partition(|d| d.is_retired());
    let settled: Vec<(DialogId, RequestRecord)> = live
        .iter()
        .chain(&retired)
        .flat_map(|d| d.requests.iter().filter(|r| owed(r)).map(|r| (d.id(), r.clone())))
        .collect();
    update_leg(call, leg_id, |l| {
        for d in l.dialogs.iter_mut() {
            // A leg's end leaves a retired dialog its own records alone.
            let own_only = owes == Owes::AtLegEnd && d.is_retired();
            d.requests.retain(|r| if own_only { r.is_own() } else { !owed(r) });
        }
    });
    for (dialog, record) in &settled {
        let (status, reason, originator) = if record.is_held() {
            (ENDED_HELD.0, ENDED_HELD.1, leg_id.to_string())
        } else {
            let (status, reason) = row_of(&record.method).owed_at_leg_end;
            (status, reason, relay_originator(call, leg_id, dialog))
        };
        answer_received(call, fx, &originator, record, status, reason, Vec::new(), None);
    }
    settled.iter().any(|(_, r)| r.is_held())
}

/// The leg a relay toward `leg_id`'s dialog `dialog` came from: the one its
/// final would have been relayed to; the caller's where none resolves.
pub(super) fn relay_originator(call: &Call, leg_id: &str, dialog: &DialogId) -> String {
    match call::helpers::resolve_relay_peer(call, leg_id, Some(dialog)) {
        RelayPeer::Leg { leg_id, .. } => leg_id,
        RelayPeer::NoLeg | RelayPeer::Unadopted => call.a_leg.leg_id.clone(),
    }
}
