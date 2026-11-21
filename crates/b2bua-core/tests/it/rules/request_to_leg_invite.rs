//! An in-dialog INVITE a service asks through `SendRequestToLeg` is the
//! stack's re-INVITE: it becomes the dialog's INVITE record, which its 2xx's
//! ACK echoes (RFC 3261 §13.2.2.4), and holds the dialog against a second
//! INVITE until it ends (§14.1).

use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;

const OFFER: &str = "v=0\r\no=b2bua 1 2 IN IP4 192.0.2.9\r\ns=-\r\nc=IN IP4 192.0.2.9\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

/// An in-dialog INVITE toward the caller offering [`OFFER`].
fn invite_to_caller() -> RuleAction {
    RuleAction::SendRequestToLeg {
        leg_id: "a".into(),
        method: "INVITE".into(),
        body: Some(Body::own(OFFER.as_bytes().to_vec(), Some("application/sdp".into()))),
        headers: vec![],
    }
}

/// `actions` run on an answered, ACKed call, and the turn's notes.
fn run(actions: &[RuleAction]) -> (HandlerResult, TurnScope) {
    let call = call_answered_and_acked();
    let event = CallEvent::sip(SipMessage::Request(invite()), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(&call, &event, "b-1", &config, &mut id_gen, actions);
    (result, id_gen)
}

/// The INVITEs the turn sent.
fn invites(result: &HandlerResult) -> Vec<SipRequest> {
    result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method() == Method::Invite => Some(r.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_in_dialog_invite_becomes_the_dialogs_invite_record() {
    let (result, id_gen) = run(&[invite_to_caller()]);
    assert!(id_gen.notes.defects().is_empty(), "{:?}", id_gen.notes.defects());
    let [invite] = invites(&result).try_into().expect("one INVITE leaves");
    let dialog = result.call.a_leg.dialogs.confirmed().expect("the caller's dialog");
    let record = dialog.sent_invite.as_ref().expect("the dialog holds the INVITE's record");
    assert_eq!(record.method, "INVITE");
    assert_eq!(record.origin, call::RequestOrigin::Own);
    assert_eq!(record.outbound_cseq, i64::from(invite.cseq().seq()), "its ACK's CSeq");
    assert_eq!(dialog.our_cseq, record.outbound_cseq, "the dialog's CSeq advanced to it");
    let held = record.sent.as_ref().expect("its client transaction is held");
    assert_eq!(Some(held.branch.as_str()), invite.top_via().branch(), "the transaction that left");
    assert!(call::helpers::invite_untaken(dialog), "no 2xx taken yet");
}

#[test]
fn a_re_invite_after_it_in_the_same_turn_is_not_sent() {
    let reinvite = RuleAction::SendReinvite {
        leg_id: "a".into(),
        body: Some(Body::own(OFFER.as_bytes().to_vec(), Some("application/sdp".into()))),
        add_headers: vec![],
    };
    let (result, id_gen) = run(&[invite_to_caller(), reinvite]);
    assert_eq!(invites(&result).len(), 1, "one INVITE in progress per dialog");
    let defects: Vec<_> = id_gen.notes.defects().iter().map(|d| d.name).collect();
    assert_eq!(defects, [b2bua_core::turn_notes::ONE_INVITE_PER_DIALOG]);
}
