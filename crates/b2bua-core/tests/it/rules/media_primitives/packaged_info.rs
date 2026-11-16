//! An unadopted media leg's `INFO` naming an Info Package: no rule of its
//! service took it, and the stack accepted no package on that dialog, so it
//! draws 469 stating the empty `Recv-Info` set (RFC 6086 §4.2.2), not the 403
//! a legacy `INFO` draws.

use super::*;

fn packaged_info() -> SipRequest {
    in_dialog_info()
        .thaw()
        .push_raw(HeaderName::from("Info-Package"), "foo")
        .freeze()
        .expect("the INFO with its package line is complete")
}

#[test]
fn a_packaged_info_from_an_unadopted_leg_is_a_bad_package() {
    let mut call = test_call();
    give_a_leg_dialog(&mut call);
    call::helpers::add_b_leg(&mut call, confirmed_b_leg("b-1", LegKind::Media));
    let event =
        CallEvent::sip(SipMessage::Request(packaged_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
    );
    let responses: Vec<_> = result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Response(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(responses.len(), 1, "one final, nothing relayed");
    assert_eq!(responses[0].status(), 469);
    let recv_info: Vec<&str> = responses[0].raw(HeaderName::from("Recv-Info")).collect();
    assert_eq!(recv_info, [""]);
}
