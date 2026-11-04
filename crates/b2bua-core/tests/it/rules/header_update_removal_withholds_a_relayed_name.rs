// ── header_updates removal beats the §16.6 relay on an originated request ───
//
// `(name, None)` states that a name does not ride. On a response the mint sites
// already honour it; on an originated request the §16.6 relay would otherwise
// re-add the originator's own copy, so a caller asking for a header to be
// withheld would see it travel anyway. The removal owns the name on every path.
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use sip_message::{HeaderName, SipHeader};

/// An a-leg INVITE carrying a vendor annotation the originator sent.
fn invite_with_vendor_header() -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(uri_of("sip:bob@127.0.0.1:5070")),
        call_id: "c1@alice".into(),
        from: Some(
            header::From::from_uri(uri_of("sip:alice@host")).with_tag(SipStr::from_static("atag")),
        ),
        to: Some(header::To::from_uri(uri_of("sip:bob@host"))),
        cseq: 1,
        via: Some(Via::udp("127.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKalice"))),
        contact: Some(header::Contact::from_uri(
            Uri::sip_user("alice", "127.0.0.1").with_port(5060),
        )),
        max_forwards: Some(70),
        body: b"v=0\r\n".to_vec(),
        content_type: None,
        extra_headers: vec![
            SipHeader { name: "X-Vendor-Term".into(), value: "sbc.example".into() },
            SipHeader { name: "P-Kept".into(), value: "rides-on".into() },
        ],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts).expect("an INVITE")
}

/// The b-leg INVITE a `CreateLeg` carrying `header_updates` emits.
fn b_leg_invite(header_updates: Vec<(String, Option<String>)>) -> SipRequest {
    let config = B2buaConfig::default();
    let a_invite = invite_with_vendor_header();
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    let call = build_initial_call(&a_invite, src, &config, &mut sip_ids::TurnIds::fixed(1), 0);
    let event = CallEvent::sip(SipMessage::Request(a_invite), src);
    let ctx = context(&call, &event, "a", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let create = RuleAction::CreateLeg {
        destination: ("10.0.1.5".into(), 5070), // IP literal → admission passes
        new_ruri: None,
        new_from: None,
        new_to: None,
        callback_context: None,
        body_override: None,
        header_updates,
        header_adds: vec![],
        kind: None,
        attempt: None,
    };
    let result = exec.execute(&[create], &call, &ctx);
    match &result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "INVITE"))
        .expect("CreateLeg emits a b-leg INVITE")
        .body
    {
        OutboundBody::Request(r) => r.clone(),
        _ => unreachable!(),
    }
}

/// Without a removal the annotation rides on — the §16.6 default this test
/// exists to keep honest, so the removal case cannot pass vacuously.
#[test]
fn the_relay_carries_the_name_when_nothing_removes_it() {
    let inv = b_leg_invite(vec![]);
    assert_eq!(inv.raw(HeaderName::from("X-Vendor-Term")).next(), Some("sbc.example"));
    assert_eq!(inv.raw(HeaderName::from("P-Kept")).next(), Some("rides-on"));
}

#[test]
fn a_removal_withholds_the_name_and_leaves_every_other_relayed_header() {
    let inv = b_leg_invite(vec![("X-Vendor-Term".into(), None)]);
    assert_eq!(
        inv.raw(HeaderName::from("X-Vendor-Term")).next(),
        None,
        "a removal beats the relayed copy of the same name"
    );
    assert_eq!(
        inv.raw(HeaderName::from("P-Kept")).next(),
        Some("rides-on"),
        "and withholds nothing else"
    );
}

/// Case-insensitively, since a name is a header name and not a string.
#[test]
fn the_removal_matches_the_name_however_it_is_spelled() {
    let inv = b_leg_invite(vec![("x-VENDOR-term".into(), None)]);
    assert_eq!(inv.raw(HeaderName::from("X-Vendor-Term")).next(), None);
}
