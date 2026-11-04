//! b-leg route-set capture (RFC 3261 §12.1.2): the b-leg is a UAC dialog, so
//! `ConfirmDialog` builds its route set from the dialog-creating 2xx's
//! Record-Route in reverse order, and a CANCEL follows the INVITE's route set
//! and next hop.

use super::fixtures::*;
use super::*;

#[test]
fn confirm_dialog_captures_b_leg_route_set_from_2xx_record_route_reversed() {
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_pending());

    // 2xx with two Record-Routes (top-to-bottom: proxy-b then proxy-a). §12.1.2:
    // the UAC route set is this list *reversed* → [proxy-a, proxy-b].
    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: <sip:proxy-b.example:5060;lr>\r\n\
Record-Route: <sip:proxy-a.example:5060;lr>\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n";
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "b-1", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);

    let d = result.call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
    assert_eq!(
        d.route_set,
        vec![
            "<sip:proxy-a.example:5060;lr>".to_string(),
            "<sip:proxy-b.example:5060;lr>".to_string(),
        ],
        "b-leg route set must be the 2xx Record-Route in reverse order (§12.1.2)"
    );
    // The other 2xx-learned fields stay correct alongside the new capture.
    assert_eq!(d.peer_tag, "bobtag");
    assert_eq!(d.remote_target, "sip:bob@10.0.0.2:5070");
}

// The front proxy's two Record-Route halves (cookie + `;outbound`) arrive on the
// wire COMBINED in a single Record-Route header (RFC 3261 §7.3.1). The §12.1.2
// reversal must operate on individual route URIs, not header lines — otherwise
// reversing one combined value is a no-op and leaves the cookie on top, so the
// worker→callee keepalive carries the cookie first and the proxy bounces it back
// to a worker after a reboot (no `;outbound` rescue). The b-leg route set MUST end up `;outbound`
// first so direction is intrinsic to the proxy's own Record-Route.
#[test]
fn confirm_dialog_splits_combined_record_route_and_puts_outbound_first() {
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_pending());

    // Worker-outbound b-leg INVITE → the proxy inserts [cookie, outbound]; the 2xx
    // echoes them comma-COMBINED in one header. §12.1.2 reverse of the individual
    // URIs → [outbound, cookie].
    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: <sip:10.0.0.9:5060;e=0;kid=k0;sig=ABC;v=3;w_bak=w1;w_pri=w0;lr>, <sip:10.0.0.9:5060;outbound;lr>\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n";
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "b-1", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);
    let rs = &result.call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog").route_set;
    assert_eq!(rs.len(), 2, "combined header must be split into 2 individual routes: {rs:?}");
    assert!(
        rs[0].contains("outbound"),
        "the proxy's `;outbound` half MUST be on top of the worker's b-leg route set (got {:?})",
        rs[0]
    );
    assert!(rs[1].contains("w_pri="), "the cookie half is second (got {:?})", rs[1]);
}

#[test]
fn confirm_dialog_without_record_route_leaves_route_set_empty() {
    // No Record-Route on the 2xx (single-hop, no record-routing proxy) → the
    // b-leg route set stays empty and in-dialog egress uses the remote target /
    // outbound-proxy fallback. Guards against clobbering with a bogus entry.
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_pending());

    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n";
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "b-1", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);

    assert!(
        result.call.b_legs[0]
            .dialogs
            .confirmed()
            .expect("the confirmed dialog")
            .route_set
            .is_empty(),
        "no Record-Route on the 2xx → b-leg route set stays empty"
    );
}

// ── CANCEL follows the INVITE's route set + next hop (RFC 3261 §9.1) ─────────
//
// Via-LB topology (`Front::Proxied`): the b-leg egresses through the front proxy, so the
// b-leg INVITE carries a preloaded outbound-proxy Route and its wire destination is the
// proxy. When the B2BUA later cancels the pending INVITE (no-answer teardown /
// reroute-on-486), §9.1 requires the CANCEL to take the SAME path — same next hop (the
// proxy, NOT `leg.source`, the callee's advertised address) and the same Route set, with
// the transaction-correlation Via branch echoed verbatim. A CANCEL sent to `leg.source`
// or without the Route bypasses the proxy and never reaches the pending server txn.
#[test]
fn cancel_follows_invite_route_set_and_next_hop_through_the_outbound_proxy() {
    use b2bua_core::effects::OutboundBody;

    let config = B2buaConfig {
        front: b2bua_core::config::Front::single_face("proxy.example", 5060),
        ..B2buaConfig::default()
    };

    let mut call = test_call();
    let a_invite = crate::caller_of(&call);
    let mut id_gen = scope(7);
    // The callee's own address — what `leg.source` becomes; the CANCEL must NOT
    // go here (it must go to the proxy).
    let callee_dest = ("10.0.0.2".to_string(), 5070u16);
    let (leg, invite_effect) = b2bua_core::rules::relay::build_b_leg(
        b2bua_core::rules::relay::CallMarks::of(&call, b2bua_core::owners::Owners::none()),
        "b-1",
        &a_invite,
        callee_dest.clone(),
        None,
        None,
        None,
        &config,
        &mut id_gen.ids,
        None,
        &[],
        &CapabilitySet::default(),
        None, // no charging vector
        &[],
        &[], // no withheld option tags
        None,
        true,
        0,
        &mut Vec::new(),
    )
    .expect("no identity rewrites, so nothing to refuse");
    // Sanity: the topology is via-LB — the INVITE itself went to the proxy.
    assert_eq!(
        invite_effect.destination,
        ("proxy.example".to_string(), 5060),
        "b-leg INVITE egresses through the front proxy"
    );
    let invite_via = match &invite_effect.body {
        OutboundBody::Request(r) => {
            r.raw(HeaderName::Via).next().expect("INVITE has a Via").to_string()
        }
        _ => panic!("INVITE is a request"),
    };

    call::helpers::add_b_leg(&mut call, leg);

    // CancelLeg is action-driven and does not read the event; any event works.
    let event = CallEvent::Timer {
        key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = context(&call, &event, "b-1", &config);
    let mut exec = executor(&config, &mut id_gen);
    let result = exec.execute(&[RuleAction::CancelLeg { leg_id: "b-1".into() }], &call, &ctx);

    let cancel_effect = result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "CANCEL"))
        .expect("a CANCEL was emitted");

    // (b) The CANCEL arrives AT THE PROXY (the INVITE's next hop), not pod-direct
    // at the callee (`leg.source`).
    assert_eq!(
        cancel_effect.destination,
        ("proxy.example".to_string(), 5060),
        "CANCEL must follow the INVITE's next hop (the proxy), not leg.source {callee_dest:?}"
    );

    let cancel = match &cancel_effect.body {
        OutboundBody::Request(r) => r,
        _ => unreachable!(),
    };
    // (a) The CANCEL carries the INVITE's Route set (the preloaded proxy Route).
    let routes: Vec<&str> = cancel.raw(HeaderName::Route).collect();
    assert_eq!(
        routes,
        vec!["<sip:proxy.example:5060;lr>"],
        "CANCEL must echo the INVITE's preloaded outbound-proxy Route (RFC 3261 §9.1)"
    );
    // ... and the transaction-correlation Via branch is the INVITE's verbatim.
    let cancel_via = cancel.raw(HeaderName::Via).next().expect("CANCEL has a Via").to_string();
    assert_eq!(cancel_via, invite_via, "CANCEL top Via (incl. branch) must equal the INVITE's");
}
