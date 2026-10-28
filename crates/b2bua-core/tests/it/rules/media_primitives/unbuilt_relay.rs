//! A relayed in-dialog request whose copy toward the peer dialog does not
//! build (the dialog's remote target fails the Request-URI checks): the
//! sender is answered 500 (RFC 3261 §21.5.1), not left to run out Timer F,
//! and the call records nothing of the relay.

use super::*;

#[test]
fn an_unbuildable_relay_answers_its_sender_and_records_nothing() {
    let mut call = test_call();
    give_a_leg_dialog(&mut call);
    let mut b = confirmed_b_leg("b-1", LegKind::Destination);
    for d in b.dialogs.iter_mut() {
        d.remote_target = "sip:bob@10.0.0.2:5070x".into();
    }
    call::helpers::add_b_leg(&mut call, b);
    call::helpers::merge_leg(&mut call, "a", "b-1");
    let peer = |c: &call::Call| {
        let dialogs = c.b_legs[0].dialogs.iter();
        dialogs.map(|d| (d.our_cseq, d.requests.len())).collect::<Vec<_>>()
    };
    let before = peer(&call);
    let event = CallEvent::sip(
        SipMessage::Request(super::super::fixtures::in_dialog_request(Method::Info)),
        "127.0.0.1:5060".parse().unwrap(),
    );
    let (config, mut id_gen) = (B2buaConfig::default(), scope(1));
    let relay = [RuleAction::RelayToPeer { transform: MessageTransform::default() }];
    let result = exec_on(&call, &event, "a", &config, &mut id_gen, &relay);

    assert_eq!(answered(&result), vec![500], "the sender is answered");
    assert_eq!(result.effects.outbound.len(), 1, "nothing is relayed toward the peer");
    assert_eq!(peer(&result.call), before, "the peer dialog spends no number, holds no relay");
}
