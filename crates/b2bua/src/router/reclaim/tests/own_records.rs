//! The request records a union keeps ([`own_records`]): those of the live
//! copy this worker made; an INVITE relay another worker made is left to it,
//! and the dialog holds again the INVITE before it.

use call::{
    AckAhead, CallModelState, InviteExchange, ReAckedCopy, RelayOwner, Repeated, RequestRecord,
    RetainedEmission,
};

use super::super::diverged::own_records;
use super::answered;

/// The ACK of the caller's 2xx to the INVITE numbered 1, as it left, with
/// three copies of that 2xx counted, the last re-ACKed at 9 s.
fn ack_of_the_first_2xx() -> AckAhead {
    AckAhead {
        ack: RetainedEmission::on_trigger(
            b"ACK sip:alice@192.0.2.10 SIP/2.0\r\n\r\n".to_vec(),
            ("192.0.2.10".into(), 5060),
            Repeated::request("ACK"),
        ),
        invite_cseq: 1,
        ack_branch: "z9hG4bKfirst-ack".into(),
        copies_received: 3,
        re_acked_copy: Some(ReAckedCopy { copy: 3, at_ms: 9_000 }),
    }
}

/// **A dropped INVITE relay gives back the INVITE before it.** The caller's
/// dialog holds a re-INVITE (CSeq 2) another worker relays, awaiting its
/// final, which carries ahead the ACK of the caller's 2xx to CSeq 1. The
/// union this worker keeps leaves that relay to its worker, its round not
/// noted ended (a 2xx to it is no late 2xx to drop), and holds CSeq 1 again,
/// with its ACK, its client transaction and its copies' count, so a copy of
/// that 2xx still draws its ACK here (RFC 3261 §13.2.2.4) under the same
/// bound (ADR-0057 §10).
#[test]
fn a_dropped_invite_relay_gives_back_the_invite_before_it() {
    let mut copy = answered(6, 1, CallModelState::Active).call;
    let d = copy.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    let mut relay = RequestRecord::relayed("INVITE", 2, 7);
    relay.relayed_by = RelayOwner::new("w1", "the other worker");
    relay.invite = Some(InviteExchange {
        ack_ahead: Some(Box::new(ack_of_the_first_2xx())),
        ..InviteExchange::default()
    });
    d.sent_invite = Some(relay);
    let live = copy.clone();

    let kept = own_records(copy, &live, &RelayOwner::new("w0", "this worker"));

    let d = kept.a_leg.dialogs.confirmed().expect("the caller's dialog");
    assert_eq!(d.untaken_invite, None, "the relay's round is its worker's, not ended");
    assert!(!call::helpers::invite_round_ended(d, 2), "a 2xx to the relay is no late 2xx");
    let held = d.sent_invite.as_ref().expect("the INVITE before the dropped relay");
    assert_eq!(held.outbound_cseq, 1, "the INVITE the ACK acknowledges");
    let x = held.invite.as_ref().expect("its exchange");
    let ahead = ack_of_the_first_2xx();
    assert_eq!(x.emitted_ack.as_ref(), Some(&ahead.ack), "its ACK, as it left");
    assert_eq!(x.ack_branch.as_deref(), Some("z9hG4bKfirst-ack"), "its ACK's transaction");
    assert_eq!(x.copies_received, 3, "the copies counted");
    assert_eq!(x.re_acked_copy, ahead.re_acked_copy, "the last copy re-ACKed");
}
