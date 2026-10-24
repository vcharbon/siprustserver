//! Declared capability advertisement: call model → wire.

use super::*;

/// A call declaring a narrow set toward the originated leg and the full set
/// toward the originator (`advertise_capabilities`).
fn call_declaring_asymmetric_capabilities() -> call::Call {
    let mut call = test_call();
    let mut features = b2bua_core::decision::default_platform_features();
    features.advertise_capabilities = Some(call::features::AdvertiseCapabilitiesFeature {
        toward_originator: None,
        toward_originated: Some(call::features::AdvertisedCapabilities {
            allow: Some(["INVITE", "ACK", "CANCEL", "BYE"].iter().map(|s| s.to_string()).collect()),
            supported: Some(vec!["timer".to_string()]),
        }),
    });
    call.features = Some(features);
    call
}

/// The declared set reaches the originated leg's wire header, resolved off the
/// call's own feature activations — the model→mint-point→wire chain.
#[test]
fn a_declared_capability_set_reaches_the_originated_leg_wire_header() {
    let call = call_declaring_asymmetric_capabilities();
    let a_invite = crate::caller_of(&call);
    let (_leg, effect) = b2bua_core::rules::relay::build_b_leg(
        b2bua_core::rules::relay::CallMarks::of(&call, b2bua_core::owners::Owners::none()),
        "b-1",
        &a_invite,
        ("10.0.0.2".to_string(), 5070),
        None,
        None,
        None,
        &B2buaConfig::default(),
        &mut scope(11).ids,
        None,
        &[],
        &b2bua_core::rules::capabilities::for_leg(&call, "b-1"),
        None, // no charging vector
        &[],
        &[], // no withheld option tags
        None,
        true,
        0,
        &mut Vec::new(),
    )
    .expect("no identity rewrites, so nothing to refuse");
    let invite = match effect.body {
        b2bua_core::effects::OutboundBody::Request(r) => r,
        b2bua_core::effects::OutboundBody::Response(_)
        | b2bua_core::effects::OutboundBody::Datagram(_) => {
            panic!("b-leg effect must carry a request")
        }
    };
    let allow = invite.raw_text(HeaderName::Allow).next().map(|v| v.as_str().to_string());
    let supported = invite.raw_text(HeaderName::Supported).next().map(|v| v.as_str().to_string());
    assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE"));
    assert_eq!(supported.as_deref(), Some("timer"));
}

/// The undeclared face states nothing of its own, so one declaration cannot
/// reach the other side of the bridge.
#[test]
fn the_undeclared_face_of_a_declaring_call_states_nothing() {
    let call = call_declaring_asymmetric_capabilities();
    use b2bua_core::rules::capabilities::{self, Face};
    assert_eq!(capabilities::declared(&call, Face::Originator), None, "nothing declared here");
    assert_eq!(capabilities::for_leg(&call, "a"), CapabilitySet::silent());
    assert!(capabilities::declared(&call, Face::Originated).is_some());
    assert_ne!(capabilities::for_leg(&call, "b-1"), CapabilitySet::silent());
}

/// A call that declares nothing states no set of its own on either face.
#[test]
fn an_undeclared_call_states_nothing_on_every_face() {
    let call = test_call();
    assert_eq!(b2bua_core::rules::capabilities::for_leg(&call, "a"), CapabilitySet::silent());
    assert_eq!(b2bua_core::rules::capabilities::for_leg(&call, "b-1"), CapabilitySet::silent());
}
