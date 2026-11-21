//! A line a decision states on the INVITE [`build_b_leg`] mints that cannot
//! take its place there — a header set, a charging host or a part line the
//! wire cannot carry whole (ADR-0053), a part line that is no entity header of
//! a part lifted to be the body (RFC 2045 §9), a header the stack binds — is
//! left out and handed back; the INVITE leaves without it.

use call::features::ChargingVectorFeature;
use sip_ids::TurnIds;
use sip_message::generators::CapabilitySet;
use sip_message::parser::custom::CustomParser;
use sip_message::write_check::WriteRefusal;
use sip_message::{HeaderName, MultipartPart, SipMessage, SipParser, SipRequest};

use super::advert::advertisement_tests::a_leg_invite_carrying;
use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::rules::relay::{build_b_leg, CallMarks};
use b2bua_sdk::model::Body;

const SPLITTING: &str = "ok\r\nVia: SIP/2.0/UDP 192.0.2.66:5060;branch=z9hG4bKinjected";

/// The INVITE minted with `header_updates`, `charging` and `body`, and what
/// was left out of it.
fn minted(
    header_updates: &[(String, Option<String>)],
    charging: Option<&ChargingVectorFeature>,
    body: Option<&Body>,
) -> (SipRequest, Vec<WriteRefusal>) {
    minted_from(&a_leg_invite_carrying(&[]), header_updates, charging, body)
}

/// [`minted`] for the originator's INVITE `a_leg_invite`.
fn minted_from(
    a_leg_invite: &SipRequest,
    header_updates: &[(String, Option<String>)],
    charging: Option<&ChargingVectorFeature>,
    body: Option<&Body>,
) -> (SipRequest, Vec<WriteRefusal>) {
    let mut skipped = Vec::new();
    let (_leg, effect) = build_b_leg(
        CallMarks {
            call_ref: "w0|call-ref|xyz",
            incarnation_mark: "k",
            is_emergency: false,
            owners: crate::owners::Owners::none(),
        },
        "b-1",
        a_leg_invite,
        ("192.0.2.7".to_string(), 5060),
        None,
        None,
        None,
        &B2buaConfig::default(),
        &mut TurnIds::fixed(0x643),
        body,
        header_updates,
        &CapabilitySet::default(),
        charging,
        &[],
        &[],
        None,
        true,
        0,
        &mut skipped,
    )
    .expect("no identity rewrites, so nothing to refuse");
    match effect.body {
        OutboundBody::Request(r) => (r, skipped),
        OutboundBody::Response(_) | OutboundBody::Datagram(_) => panic!("an INVITE"),
    }
}

fn lines(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

#[test]
fn a_set_with_a_line_break_is_left_out_and_the_others_ride() {
    let updates = [
        ("X-Split".to_string(), Some(SPLITTING.to_string())),
        ("X/Bad".to_string(), Some("v".to_string())),
        ("X-Kept".to_string(), Some("fine".to_string())),
    ];
    let (invite, skipped) = minted(&updates, None, None);
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(lines(&invite, "X-Split").is_empty());
    assert_eq!(lines(&invite, "X-Kept"), ["fine"]);
    assert_eq!(invite.via().len(), 1);
    assert!(!String::from_utf8_lossy(invite.image()).contains("z9hG4bKinjected"));
}

#[test]
fn a_charging_host_with_a_line_break_mints_no_vector() {
    let arm =
        ChargingVectorFeature { generated_at: Some(SPLITTING.to_string()), ..Default::default() };
    let (invite, skipped) = minted(&[], Some(&arm), None);
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(lines(&invite, "P-Charging-Vector").is_empty());
}

#[test]
fn a_part_line_with_a_line_break_is_left_out_of_the_body() {
    let parts = vec![
        MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
            .with_header("Content-ID", "<loc@example.invalid>")
            .with_header("X-Note", SPLITTING),
        MultipartPart::new("application/vnd.example.data", vec![0x77, 0x15]),
    ];
    let body = Body::from_leg(Vec::new(), "a".to_string()).with_parts(parts);
    let (invite, skipped) = minted(&[], None, Some(&body));
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    let text = String::from_utf8_lossy(invite.body());
    assert!(text.contains("<loc@example.invalid>"), "{text}");
    assert!(!text.contains("z9hG4bKinjected"), "{text}");
}

/// A part that is the whole body brings only its `Content-` lines onto the
/// INVITE (RFC 2045 §9): a `Replaces` or `Authorization` in its head is left
/// out and handed back, never a request header.
#[test]
fn a_lifted_part_brings_no_request_header() {
    let parts = vec![MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
        .with_header("Content-ID", "<loc@example.invalid>")
        .with_header("Replaces", "abc@192.0.2.1;to-tag=1;from-tag=2")
        .with_header("Authorization", "Digest username=\"u\"")];
    let body = Body::from_leg(Vec::new(), "a".to_string()).with_parts(parts);
    let (invite, skipped) = minted(&[], None, Some(&body));
    assert!(lines(&invite, "Replaces").is_empty());
    assert!(lines(&invite, "Authorization").is_empty());
    assert_eq!(lines(&invite, "Content-ID"), ["<loc@example.invalid>"]);
    assert_eq!(&invite.body()[..], b"<presence/>");
    assert_eq!(skipped.len(), 2, "{skipped:?}");
}

/// Whether one of `skipped` names the header `name`.
fn refuses(skipped: &[WriteRefusal], name: &str) -> bool {
    skipped.iter().any(|refusal| refusal.reason.contains(name))
}

/// A header the transaction, a negotiation, an authentication exchange, a
/// dialog reference or the body binds is the stack's to state on the INVITE
/// it mints (RFC 3261 §16.6, §20): a decision's set of one is left out and
/// handed back, so the minted body keeps its own `Content-Type`. The
/// originator's session interval still rides, and so does a capability set
/// the mint narrows.
#[test]
fn a_bound_header_a_decision_sets_is_left_out_of_the_minted_invite() {
    let updates = [
        ("Require", "zz-ext"),
        ("Authorization", "Digest username=\"mallory\""),
        ("Replaces", "other@192.0.2.1;to-tag=t;from-tag=f"),
        ("Content-Type", "text/plain"),
        ("Allow", "INVITE, ACK, BYE"),
        ("X-Kept", "fine"),
    ]
    .map(|(name, value)| (name.to_string(), Some(value.to_string())));
    let a = a_leg_invite_carrying(&[("Session-Expires", "1800;refresher=uac")]);
    let sdp = b"v=0\r\no=- 1 1 IN IP4 192.0.2.5\r\ns=-\r\nt=0 0\r\n".to_vec();
    let body = Body::own(sdp.clone(), None);
    let (invite, skipped) = minted_from(&a, &updates, None, Some(&body));
    for name in ["Require", "Authorization", "Replaces"] {
        assert!(lines(&invite, name).is_empty(), "{name} rides: {:?}", invite.headers());
    }
    assert_eq!(lines(&invite, "Content-Type"), ["application/sdp"]);
    assert_eq!(&invite.body()[..], &sdp[..]);
    assert_eq!(lines(&invite, "Session-Expires"), ["1800;refresher=uac"]);
    assert_eq!(lines(&invite, "Allow"), ["INVITE, ACK, BYE"]);
    assert_eq!(lines(&invite, "X-Kept"), ["fine"]);
    assert_eq!(skipped.len(), 4, "{skipped:?}");
    for name in ["Require", "Authorization", "Replaces", "Content-Type"] {
        assert!(refuses(&skipped, name), "{name} is handed back: {skipped:?}");
    }
}

/// A decision's removal of a line describing the body the INVITE relays whole
/// is refused like a set (RFC 3261 §20.11): the line still describes the
/// body, the removal handed back.
#[test]
fn a_bound_header_a_decision_removes_still_describes_the_relayed_body() {
    let sdp = "v=0\r\no=- 1 1 IN IP4 192.0.2.5\r\ns=-\r\nt=0 0\r\n";
    let raw = format!(
        "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
Content-Type: application/sdp\r\n\
Content-Disposition: session;handling=required\r\n\
Content-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let a = match CustomParser::new().parse(raw.as_bytes()).expect("the fixture INVITE parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("a request"),
    };
    let updates = [("Content-Disposition".to_string(), None)];
    let (invite, skipped) = minted_from(&a, &updates, None, None);
    assert_eq!(lines(&invite, "Content-Disposition"), ["session;handling=required"]);
    assert_eq!(&invite.body()[..], sdp.as_bytes());
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(refuses(&skipped, "Content-Disposition"), "{skipped:?}");
}

/// The leg minted with `new_from` / `new_to` stated, and what was left out.
fn rewritten(
    new_from: &str,
    new_to: &str,
) -> Result<(SipRequest, Vec<WriteRefusal>), super::UnreadableAddress> {
    let mut skipped = Vec::new();
    let (_leg, effect) = build_b_leg(
        CallMarks {
            call_ref: "w0|call-ref|xyz",
            incarnation_mark: "k",
            is_emergency: false,
            owners: crate::owners::Owners::none(),
        },
        "b-1",
        &a_leg_invite_carrying(&[]),
        ("192.0.2.7".to_string(), 5060),
        None,
        Some(new_from),
        Some(new_to),
        &B2buaConfig::default(),
        &mut TurnIds::fixed(0x643),
        None,
        &[],
        &CapabilitySet::default(),
        None,
        &[],
        &[],
        None,
        true,
        0,
        &mut skipped,
    )?;
    match effect.body {
        OutboundBody::Request(r) => Ok((r, skipped)),
        OutboundBody::Response(_) | OutboundBody::Datagram(_) => panic!("an INVITE"),
    }
}

/// An identity replace the wire cannot carry keeps the relayed identity and
/// is handed back; the other replace applies. One that does not read still
/// refuses the leg.
#[test]
fn an_identity_with_a_line_break_keeps_the_relayed_one() {
    let a = a_leg_invite_carrying(&[]);
    let split = format!("\"x{SPLITTING}\" <sip:mallory@example.com>");
    let (invite, skipped) = rewritten(&split, "<sip:bob@example.com>").expect("the leg is built");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(invite.from().uri(), a.from().uri(), "the relayed identity stands");
    assert_eq!(invite.from().display(), None);
    assert_eq!(invite.to().uri().user(), Some("bob"));
    assert_eq!(invite.via().len(), 1);
    assert!(rewritten("<sip:mallory@example.com", "<sip:bob@example.com>").is_err());
}
