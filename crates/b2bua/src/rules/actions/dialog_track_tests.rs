//! The route sets the a-leg and b-leg dialogs record, and the caller-facing
//! dialog a rejected caller never gains.

use super::*;
use crate::config::B2buaConfig;
use crate::turn_scope::TurnScope;
use sip_ids::TurnIds;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const PROXY_ROUTE: &str = "<sip:10.0.0.9:5060;lr>";

/// A port no reader accepts (RFC 3261 §19.1.1 `port` is 16-bit): the message
/// parses, the recorded route does not.
const UNREADABLE_RR: &str = "<sip:10.0.0.9:70596;lr>";

fn ok_200(record_route: &str) -> sip_message::SipResponse {
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=bob\r\n\
Call-ID: c1@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: {record_route}\r\n\
Contact: <sip:bob@10.244.2.7:5060>\r\n\
Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("parses") {
        SipMessage::Response(r) => r,
        _ => panic!("expected response"),
    }
}

fn invite(record_route: &str) -> sip_message::SipRequest {
    let raw = format!(
        "INVITE sip:svc@10.244.1.5:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKa\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@10.0.0.1:5060>;tag=alice\r\n\
To: <sip:svc@10.0.0.9:5060>\r\n\
Call-ID: c1@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: {record_route}\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("parses") {
        SipMessage::Request(r) => r,
        _ => panic!("expected request"),
    }
}

fn proxied_config() -> B2buaConfig {
    B2buaConfig { b2b_outbound_proxy: Some(("10.0.0.9".to_string(), 5060)), ..Default::default() }
}

// A recorded route no reader accepts must never leave the dialog with an
// EMPTY route set: an empty set sends in-dialog requests at the peer's
// Contact (pod-direct), which the deployment forbids. Both dialog sides —
// the UAC route set read off a 2xx/reliable 1xx and the UAS route set read
// off the a-leg INVITE — fall back to the configured front proxy instead.
#[test]
fn an_unreadable_record_route_never_yields_an_empty_route_set() {
    let config = proxied_config();
    let mut scope = TurnScope::of(TurnIds::fixed(0xD1));
    let caller = invite(PROXY_ROUTE);
    let mut exec =
        ActionExecutor { config: &config, turn: &mut scope, now_ms: 0, caller_invite: &caller };

    let resp = ok_200(UNREADABLE_RR);
    assert!(uac_route_set(&resp).is_err(), "the fixture's recorded route must not read");
    assert_eq!(
        exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1"),
        vec![PROXY_ROUTE.to_string()],
        "UAC side: an unreadable recorded route routes via the front proxy, not pod-direct",
    );

    let req = invite(UNREADABLE_RR);
    assert!(uas_route_set(&req).is_err(), "the fixture's recorded route must not read");
    assert_eq!(
        exec.dialog_route_set(uas_route_set(&req), "call-1", "a"),
        vec![PROXY_ROUTE.to_string()],
        "UAS side: same fallback",
    );
}

// With no front proxy configured (local/dev) the transport IS peer-direct,
// so the fallback is empty — but the read still fails loudly rather than
// being mistaken for "the peer recorded no route".
#[test]
fn without_a_front_proxy_the_fallback_is_empty_but_the_read_still_fails() {
    let config = B2buaConfig::default();
    let mut scope = TurnScope::of(TurnIds::fixed(0xD2));
    let caller = invite(PROXY_ROUTE);
    let mut exec =
        ActionExecutor { config: &config, turn: &mut scope, now_ms: 0, caller_invite: &caller };
    let resp = ok_200(UNREADABLE_RR);
    assert!(uac_route_set(&resp).is_err());
    assert!(exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1").is_empty());
}

// The readable path is unchanged: the UAC reverses the recorded routes
// (§12.1.2), the UAS keeps them in recorded order (§12.1.1), and a
// comma-combined line teaches both halves.
#[test]
fn readable_recorded_routes_keep_their_dialog_order() {
    let config = proxied_config();
    let mut scope = TurnScope::of(TurnIds::fixed(0xD3));
    let caller = invite(PROXY_ROUTE);
    let mut exec =
        ActionExecutor { config: &config, turn: &mut scope, now_ms: 0, caller_invite: &caller };
    let combined = "<sip:10.0.0.9:5060;outbound;lr>,<sip:10.0.0.9:5060;target=1;lr>";

    let resp = ok_200(combined);
    assert_eq!(
        exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1"),
        vec![
            "<sip:10.0.0.9:5060;target=1;lr>".to_string(),
            "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
        ],
    );

    let req = invite(combined);
    assert_eq!(
        exec.dialog_route_set(uas_route_set(&req), "call-1", "a"),
        vec![
            "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
            "<sip:10.0.0.9:5060;target=1;lr>".to_string(),
        ],
    );
}

// A caller whose INVITE took a non-2xx final holds no dialog of it
// (RFC 3261 §12.1): nothing opens one for her afterwards; before that
// final, the a-leg's own dialog is opened under the preferred tag.
#[test]
fn a_caller_rejected_with_a_non_2xx_final_gains_no_dialog() {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut scope = TurnScope::of(TurnIds::fixed(0xD4));
    let open = crate::initial_invite::build_initial_call(
        &crate::router::test_support::invite("w0", "w1", "rejected-caller"),
        crate::router::test_support::src(),
        &config,
        &mut scope.ids,
        0,
    );
    let caller = crate::router::test_support::caller_of(&open);
    let mut exec =
        ActionExecutor { config: &config, turn: &mut scope, now_ms: 0, caller_invite: &caller };

    let mut rejected = open.clone();
    rejected.a_leg.invite_final_sent = Some(487);
    exec.ensure_a_dialog_with(&mut rejected, None);
    exec.ensure_a_dialog_with(&mut rejected, Some("shown".into()));
    assert!(rejected.a_leg.dialogs.is_empty(), "a rejected caller gains no dialog");
    assert_eq!(rejected.a_leg.own_tag, None);

    let mut ringing = open;
    assert_eq!(exec.ensure_a_dialog_with(&mut ringing, Some("shown".into())), "shown");
    assert!(caller_dialog(&ringing, "shown").is_some(), "her own dialog opens before a final");
}
