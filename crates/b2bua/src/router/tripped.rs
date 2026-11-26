//! A turn that crossed a per-call cap (ADR-0057 §4) fails as a panicked turn
//! does: its result is discarded, so the call stays as the turn found it,
//! its safepoint. The safepoint is written owing the release of the call's
//! limiter key, which a discarded turn may have counted, and holding the
//! non-2xx final a dialled leg's INVITE took in the turn, which its client
//! transaction completed all the same; an in-dialog request the turn held is
//! answered a bare `500`; the trip is counted and reported to the reaper,
//! whose `fatal-error` verdict ends the call on the wire from the safepoint.
//! The cap's name reaches the counters, the log and the CDR, never the wire.

use std::sync::Arc;

use b2bua_core::caps::Tripped;
use b2bua_sdk::event::CallEvent;
use call::Call;
use sip_message::{Method, SipMessage};

use super::commit::commit_safepoint;
use super::RouterCtx;
use crate::dispatch::HandlerFailure;
use crate::effects::HandlerResult;

/// Fail `call_ref`'s turn, which `tripped`. `safepoint` is the call before
/// the turn: the resident record, or the record an initial INVITE's turn
/// wrote before its first fold. `in_hand` is the in-dialog event the turn
/// handled: a request among them is answered `500` here, a non-2xx final to
/// a dialled leg's INVITE is written on the safepoint
/// ([`record_completed_invite`]). The caller's initial INVITE is none: its
/// final is the verdict's teardown's (ADR-0022). The caller holds the
/// per-call lock.
pub(super) async fn fail_turn(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    in_hand: Option<&CallEvent>,
    tripped: Tripped,
    safepoint: Option<Call>,
    now_ms: i64,
) {
    if super::commit::fenced(ctx, call_ref) {
        return;
    }
    let cap = tripped.cap;
    ctx.metrics.record_cap_trip(cap);
    let trip = crate::lifecycle::CapTrip { call_ref: call_ref.to_string(), kind: cap.kind() };
    ctx.cap_trip_waves.record_with(cap.label(), "trips", 1, trip);
    if let Some(mut call) = safepoint {
        if crate::trace::sampled(&call) {
            let fact = crate::turn_notes::TraceFact {
                kind: "cap.tripped",
                at_ms: now_ms,
                detail: format!("{} ({}): the turn is discarded", cap.label(), cap.kind().label()),
            };
            crate::trace::emit::turn_fact(call_ref, &fact);
        }
        call.limiter.owe_release();
        if let Some(event) = in_hand {
            record_completed_invite(&mut call, event, now_ms);
        }
        let label = &"the safepoint of a turn past a per-call cap";
        commit_safepoint(ctx, call_ref, HandlerResult::new(call), now_ms, label).await;
    }
    if let Some(event) = in_hand {
        answer_500(ctx, call_ref, event).await;
    }
    ctx.reaper.report(call_ref, HandlerFailure::Tripped(tripped));
}

/// Write on `call` the final `event` brings when it is a non-2xx response to
/// the INVITE that dialled one of `call`'s legs, received at `now_ms`. Its
/// client transaction completed and ACKed it (RFC 3261 §17.1.1.3) whatever
/// became of the turn: the leg awaits no final and is owed no CANCEL (§9.1),
/// and its early dialogs end. A 2xx is not written: its ACK is the call's.
fn record_completed_invite(call: &mut Call, event: &CallEvent, now_ms: i64) {
    let CallEvent::Sip { message, .. } = event else { return };
    let SipMessage::Response(resp) = message.as_ref() else { return };
    if resp.status() < 300 || resp.cseq().method() != Method::Invite {
        return;
    }
    let call_id = resp.call_id().as_str();
    let our_tag = resp.from().tag();
    let dialled = |leg: &&call::Leg| {
        leg.call_id == call_id
            && leg.outbound_invite().is_some_and(|i| our_tag == Some(i.our_tag.as_str()))
    };
    let Some(leg_id) = call.b_legs.iter().find(dialled).map(|l| l.leg_id.clone()) else { return };
    let cseq = i64::from(resp.cseq().seq());
    call::helpers::record_dialled_invite_final(call, &leg_id, cseq, resp.status(), now_ms);
}

/// Answer the request `event` carries with the stack's bare `500`, unless it
/// is an ACK, which takes no response.
async fn answer_500(ctx: &Arc<RouterCtx>, call_ref: &str, event: &CallEvent) {
    let CallEvent::Sip { message, .. } = event else { return };
    let SipMessage::Request(req) = message.as_ref() else { return };
    if req.method() == Method::Ack {
        return;
    }
    let Ok(resp) = super::responses::build_bare_500(req) else { return };
    super::process::answer_after_the_write(ctx, call_ref, event, Some(Box::new(resp))).await;
}
