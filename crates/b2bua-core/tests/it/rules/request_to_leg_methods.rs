//! The methods `SendRequestToLeg` sends: those whose records it keeps (BYE,
//! INVITE, OPTIONS, INFO, UPDATE, MESSAGE). A NOTIFY rides its subscription
//! (RFC 6665 §8.1.2), a PRACK the reliable provisional it acknowledges
//! (RFC 3262 §7.2), a REFER the subscription it creates (RFC 3515 §2.4.4):
//! asked here, they and any other method leave nothing on the wire, leave the
//! dialog's CSeq where it was, and the turn reports the defect
//! `request-to-leg-names-a-method-it-sends`.

use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;

/// A request of `method` toward the caller's confirmed dialog.
fn request_to_caller(method: &str) -> RuleAction {
    RuleAction::SendRequestToLeg {
        leg_id: "a".into(),
        method: method.into(),
        body: None,
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

/// The methods of the requests the turn sent.
fn sent_methods(result: &HandlerResult) -> Vec<String> {
    result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(r.method().as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// The caller's dialog's CSeq after `result`.
fn caller_cseq(result: &HandlerResult) -> i64 {
    result.call.a_leg.dialogs.confirmed().expect("the caller's dialog").our_cseq
}

/// A `method` request asked toward the caller leaves nothing, takes no CSeq,
/// and the turn reports the one defect naming it and the leg.
fn refused(method: &str) {
    let before = call_answered_and_acked().a_leg.dialogs.confirmed().expect("dialog").our_cseq;
    let (result, id_gen) = run(&[request_to_caller(method)]);
    assert_eq!(sent_methods(&result), Vec::<String>::new(), "no {method} leaves");
    assert_eq!(caller_cseq(&result), before, "the {method} takes no CSeq");
    let defects: Vec<_> =
        id_gen.notes.defects().iter().map(|d| (d.name, d.detail.clone())).collect();
    let [(name, detail)] = defects.as_slice() else { panic!("one defect: {defects:?}") };
    assert_eq!(*name, b2bua_core::turn_notes::REQUEST_TO_LEG_METHOD);
    assert!(detail.contains(method) && detail.contains("leg a"), "{detail}");
}

#[test]
fn a_notify_is_refused() {
    refused("NOTIFY");
}

#[test]
fn a_prack_is_refused() {
    refused("PRACK");
}

#[test]
fn a_refer_is_refused() {
    refused("REFER");
}

#[test]
fn a_method_with_no_dialog_request_of_its_own_is_refused() {
    refused("SUBSCRIBE");
}

/// A `method` request asked toward the caller leaves at the dialog's next
/// CSeq, and the turn reports no defect.
fn sent(method: &str) {
    let before = call_answered_and_acked().a_leg.dialogs.confirmed().expect("dialog").our_cseq;
    let (result, id_gen) = run(&[request_to_caller(method)]);
    assert!(id_gen.notes.defects().is_empty(), "{:?}", id_gen.notes.defects());
    assert_eq!(sent_methods(&result), [method]);
    assert_eq!(caller_cseq(&result), before + 1);
}

#[test]
fn an_info_leaves_at_the_next_cseq() {
    sent("INFO");
}

#[test]
fn an_update_leaves_at_the_next_cseq() {
    sent("UPDATE");
}

#[test]
fn a_message_leaves_at_the_next_cseq() {
    sent("MESSAGE");
}
