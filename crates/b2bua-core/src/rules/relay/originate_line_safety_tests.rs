//! A line a decision states on the INVITE [`build_b_leg`] mints that the wire
//! cannot carry whole (ADR-0053) — a header set, a charging host, a part line
//! — is left out and handed back; the INVITE leaves without it.

use call::features::ChargingVectorFeature;
use sip_ids::TurnIds;
use sip_message::generators::CapabilitySet;
use sip_message::write_check::WriteRefusal;
use sip_message::{HeaderName, MultipartPart, SipRequest};

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
