//! What the transaction layer made true of the event a turn past a per-call
//! cap held, whatever became of the turn (ADR-0057 §4), written on the
//! call's safepoint so the verdict's teardown starts from the wire as it
//! stands: a non-2xx final to the INVITE that dialled a leg, which its
//! client transaction completed and ACKed (RFC 3261 §17.1.1.3); an INVITE's
//! `Response` timeout (nothing answered it), whose CANCEL the transaction
//! holds unsent (§9.1); the caller's CANCEL, answered 200 with her INVITE's
//! 487 (§9.2).

use b2bua_core::rules::relay::cancelled_final;
use b2bua_core::turn_notes::TurnNotes;
use b2bua_sdk::event::CallEvent;
use call::{Call, TxnTimeoutKind};
use sip_message::{Method, SipMessage, SipResponse};

/// Write on `call`, the safepoint of a discarded turn, what the transaction
/// layer made true of `event`, received at `now_ms` (module doc).
pub(super) fn record(call: &mut Call, event: &CallEvent, now_ms: i64) {
    match event {
        CallEvent::Sip { message, .. } => {
            if let SipMessage::Response(resp) = message.as_ref() {
                record_completed_invite(call, resp, now_ms);
            }
        }
        CallEvent::Timeout {
            timeout_kind: TxnTimeoutKind::Response,
            method,
            leg_id: Some(leg_id),
            cseq,
            ..
        } if method.as_deref() == Some(Method::Invite.as_str()) => {
            call::helpers::record_dead_hop(call, leg_id, i64::from(*cseq));
        }
        CallEvent::Cancelled { in_dialog: false, to_tag, .. } => {
            record_cancelled_caller(call, to_tag.clone());
        }
        _ => {}
    }
}

/// Write on `call` the final `resp` brings when it is a non-2xx response to
/// the INVITE that dialled one of `call`'s legs, received at `now_ms`: the
/// leg awaits no final and is owed no CANCEL (§9.1), and its early dialogs
/// end. A 2xx is not written: its ACK is the call's to send. A provisional
/// is not either: the leg's INVITE still rings, and is owed its CANCEL.
fn record_completed_invite(call: &mut Call, resp: &SipResponse, now_ms: i64) {
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

/// Write on `call` the 487 the transaction layer answered the caller's
/// INVITE with under `to_tag` as it took her CANCEL, retained as the turn
/// would have retained it: her INVITE has its final, and the teardown owes
/// her none.
fn record_cancelled_caller(call: &mut Call, to_tag: Option<String>) {
    let sent = cancelled_final(call, to_tag, &mut TurnNotes::default());
    let caller = call.a_leg.leg_id.clone();
    call::helpers::record_cancelled_invite(call, &caller, sent);
}
