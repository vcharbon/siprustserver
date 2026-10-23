//! RFC 7315 §5.6 charging correlation on a leg the B2BUA originates — every
//! assertion reads the serialized INVITE [`build_b_leg`] mints.

use call::features::ChargingVectorFeature;
use sip_ids::TurnIds;
use sip_message::generators::CapabilitySet;
use sip_message::header::{ChargingVector, HeaderValue};
use sip_message::{SipRequest, SipStr};

use super::advert::advertisement_tests::a_leg_invite_carrying;
use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::rules::relay::{build_b_leg, CallMarks};

/// The `P-Charging-Vector` the originated-leg INVITE carries, if any.
fn b_leg_vector(
    a_leg_invite: &SipRequest,
    charging: Option<&ChargingVectorFeature>,
) -> Option<String> {
    b_leg_vector_from(a_leg_invite, charging, &mut TurnIds::fixed(0xCAB))
}

/// The same, on a stated generator — one worker's identifier stream.
fn b_leg_vector_from(
    a_leg_invite: &SipRequest,
    charging: Option<&ChargingVectorFeature>,
    ids: &mut TurnIds,
) -> Option<String> {
    b_leg_vector_stating(a_leg_invite, charging, ids, &[])
}

/// The same, the decision stating `header_updates` on the INVITE.
fn b_leg_vector_stating(
    a_leg_invite: &SipRequest,
    charging: Option<&ChargingVectorFeature>,
    ids: &mut TurnIds,
    header_updates: &[(String, Option<String>)],
) -> Option<String> {
    let (_leg, effect) = build_b_leg(
        CallMarks { call_ref: "w0|call-ref|xyz", incarnation_mark: "k", is_emergency: false },
        "b-1",
        a_leg_invite,
        ("10.244.2.7".to_string(), 5060),
        None,
        None,
        None,
        &B2buaConfig::default(),
        ids,
        None,
        header_updates,
        &CapabilitySet::default(),
        charging,
        &[],
        &[], // no withheld option tags
        None,
        true,
        0,
        &mut Vec::new(),
    )
    .expect("no identity rewrites, so nothing to refuse");
    let invite = match effect.body {
        OutboundBody::Request(r) => r,
        OutboundBody::Response(_) | OutboundBody::Datagram(_) => {
            panic!("b-leg effect must carry a request")
        }
    };
    let name = ChargingVector::header_name();
    invite.headers().iter().find(|h| name.matches(&h.name)).map(|h| h.value.as_str().to_string())
}

/// Armed and nothing received: the element starting the leg generates the
/// identifier, stating where it generated it.
#[test]
fn an_originated_leg_carries_a_generated_identifier() {
    let value = b_leg_vector(&a_leg_invite_carrying(&[]), Some(&ChargingVectorFeature::default()))
        .expect("an armed call stamps a charging vector");
    let parsed = ChargingVector::parse(&SipStr::owned(&value)).expect("RFC 7315 §5.6 form");
    assert!(!parsed.icid_value().is_empty());
    assert_eq!(parsed.icid_generated_at(), Some(B2buaConfig::default().sip_local_ip.as_str()));
}

/// Two legs of two calls never share an identifier — it is the key the
/// records are matched on.
#[test]
fn each_originated_leg_generates_its_own_identifier() {
    let arm = ChargingVectorFeature::default();
    let mut ids = TurnIds::fixed(0xCAB);
    let first = b_leg_vector_from(&a_leg_invite_carrying(&[]), Some(&arm), &mut ids);
    let second = b_leg_vector_from(&a_leg_invite_carrying(&[]), Some(&arm), &mut ids);
    assert_ne!(first, second);
}

/// The correlation invariant: a vector the originator sent is the session's
/// identifier, relayed unchanged — an armed call never re-mints it.
#[test]
fn a_received_identifier_is_relayed_unchanged_even_when_armed() {
    let received = "icid-value=abc123;icid-generated-at=upstream.example";
    let invite = a_leg_invite_carrying(&[("P-Charging-Vector", received)]);
    assert_eq!(
        b_leg_vector(&invite, Some(&ChargingVectorFeature::default())).as_deref(),
        Some(received)
    );
}

/// A decision removing the vector from the INVITE outranks the arm: the
/// armed call mints none, and the received one is taken off.
#[test]
fn a_stated_removal_outranks_the_arm() {
    let removal = [("P-Charging-Vector".to_string(), None)];
    let arm = ChargingVectorFeature::default();
    let mut ids = TurnIds::fixed(0xCAB);
    let bare = a_leg_invite_carrying(&[]);
    assert_eq!(b_leg_vector_stating(&bare, Some(&arm), &mut ids, &removal), None);
    let carrying = a_leg_invite_carrying(&[("P-Charging-Vector", "icid-value=abc")]);
    assert_eq!(b_leg_vector_stating(&carrying, Some(&arm), &mut ids, &removal), None);
}

/// Unarmed: the stack generates none, and a received one still relays.
#[test]
fn an_unarmed_call_generates_none() {
    assert_eq!(b_leg_vector(&a_leg_invite_carrying(&[]), None), None);
    let received = "icid-value=abc123";
    let invite = a_leg_invite_carrying(&[("P-Charging-Vector", received)]);
    assert_eq!(b_leg_vector(&invite, None).as_deref(), Some(received));
}

/// The arm names the element the identifier is generated at.
#[test]
fn the_arm_names_the_generating_element() {
    let arm = ChargingVectorFeature {
        generated_at: Some("edge.example".to_string()),
        ..Default::default()
    };
    let value = b_leg_vector(&a_leg_invite_carrying(&[]), Some(&arm)).expect("armed");
    let parsed = ChargingVector::parse(&SipStr::owned(&value)).unwrap();
    assert_eq!(parsed.icid_generated_at(), Some("edge.example"));
}
