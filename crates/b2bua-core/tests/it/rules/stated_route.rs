//! An in-dialog INVITE the stack sends states, in its via-extension, the Route past
//! the proxy's own entries (ADR-0055 §12); any other request states none.

use super::fixtures::*;
use super::*;
use b2bua_core::stack_identity::read_via_extension;
use sip_ids::SigningKey;

const HOP: &str = "<sip:hop.example:5070;lr>";

fn key() -> SigningKey {
    SigningKey::new(&[5u8; 32]).expect("a 32-byte key")
}

fn config() -> B2buaConfig {
    B2buaConfig {
        cluster_key: Some(key()),
        front: b2bua_core::config::Front::single_face("10.0.0.9", 5060),
        ..B2buaConfig::default()
    }
}

/// A call with a confirmed `b-1` whose route set holds a hop past the proxy's pair.
fn call() -> call::Call {
    let mut call = super::fixtures::test_call();
    confirm_a(&mut call);
    let mut leg = super::fixtures::b_leg_pending();
    call::testkit::stage_leg(&mut leg, LegState::Confirmed);
    let dialog = with_dialog(&mut leg, "b-tag");
    dialog.peer_cseq = Some(1);
    dialog.route_set = vec![
        "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
        "<sip:192.0.2.1:5060;w_pri=b1;lr>".to_string(),
        HOP.to_string(),
    ];
    call.b_legs = vec![leg];
    call
}

/// The route the via-extension of the one request `actions` send on `b-1` states.
fn stated_route(actions: &[RuleAction]) -> String {
    let result = execute_with(&config(), &call(), actions);
    let via = result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("b-1"))
        .find_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Request(r) => Some(r.top_via().clone()),
            _ => None,
        })
        .expect("a request on the leg");
    let method = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Request(r) => Some(r.method().as_str().to_string()),
            _ => None,
        })
        .expect("a request");
    read_via_extension(&via, &method, Some(&key())).expect("a verified extension").route
}

#[test]
fn the_stacks_own_reinvite_states_the_route_past_the_proxy() {
    let route = stated_route(&[RuleAction::SendReinvite {
        leg_id: "b-1".into(),
        body: Some(Body::own(b"v=0\r\n".to_vec(), None)),
        add_headers: Vec::new(),
    }]);
    assert_eq!(route, HOP);
}

#[test]
fn a_request_sent_to_the_leg_states_it_on_an_invite_alone() {
    let sent = |method: &str| {
        stated_route(&[RuleAction::SendRequestToLeg {
            leg_id: "b-1".into(),
            method: method.into(),
            body: None,
            headers: Vec::new(),
        }])
    };
    assert_eq!(sent("INVITE"), HOP);
    assert_eq!(sent("OPTIONS"), "");
}
