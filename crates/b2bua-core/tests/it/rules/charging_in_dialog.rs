//! The stack's own re-INVITEs and the leg's charging vector (RFC 7315 §5.6).
//!
//! With `ChargingVectorFeature::in_dialog_invites`, a re-INVITE the stack sends
//! on its own behalf carries the vector of the leg it travels: the one that
//! leg's dialog-creating INVITE carried, none where it carried none.

use super::*;
use call::features::{
    ChargingVectorFeature, FeatureActivations, KeepaliveActivation, PlatformActivations,
};
use call::{ActivePeer, HostPort, InviteTxnHandle};
use sip_message::draft::Entry;

const CALLER: &str = "icid-value=caller-0001;icid-generated-at=edge.example";
const B1: &str = "icid-value=dialled-0001;icid-generated-at=10.0.0.9";
const B2: &str = "icid-value=target-0002;icid-generated-at=10.0.0.9";
const M1: &str = "icid-value=media-0003;icid-generated-at=10.0.0.9";
const STATED: &str = "icid-value=stated-0004";

fn features(option: bool, uncharged: bool) -> FeatureActivations {
    FeatureActivations {
        platform: PlatformActivations {
            max_duration_ms: 7_200_000,
            max_duration_anchor: Default::default(),
            keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 3 },
        },
        refer: None,
        relay_first_18x_to_180: None,
        no_answer_ms: None,
        advertise_capabilities: None,
        charging_vector: Some(ChargingVectorFeature {
            generated_at: None,
            in_dialog_invites: option,
        }),
        withhold_option_tags: None,
        stated_headers: None,
        uncharged_media_legs: uncharged,
        withhold_on_relayed_provisionals: None,
    }
}

/// A confirmed originated leg whose dialling INVITE carried `vector`.
fn confirmed(leg_id: &str, kind: LegKind, vector: &str) -> Leg {
    let mut leg = super::b_leg_pending();
    leg.leg_id = leg_id.into();
    leg.call_id = format!("{leg_id}@x");
    call::testkit::stage_leg(&mut leg, LegState::Confirmed);
    leg.kind = Some(kind);
    with_dialog(&mut leg, &format!("{leg_id}-tag")).peer_cseq = Some(1);
    let dialled = super::invite()
        .thaw()
        .push_raw(HeaderName::from("P-Charging-Vector"), vector.to_string())
        .freeze()
        .expect("the dialling INVITE");
    let dest = HostPort { host: "10.0.0.2".into(), port: 5070 };
    let branch = format!("z9hG4bK{leg_id}");
    let handle = InviteTxnHandle::new(&branch, 1, dialled.image().to_vec(), dest);
    leg.outbound_invite.as_mut().expect("a dialled leg").txn = Some(handle);
    leg
}

/// Alice ↔ b-1 bridged, the transfer target b-2 and the media leg m-1
/// confirmed beside them; alice's INVITE carried `caller` where given.
fn call(caller: Option<&str>, option: bool, uncharged: bool) -> call::Call {
    let mut call = super::test_call();
    if let Some(own) = caller {
        call.a_leg_invite
            .headers
            .push(call::SipHeader { name: "P-Charging-Vector".into(), value: own.into() });
    }
    confirm_a(&mut call);
    call.a_leg.dialogs = vec![super::a_dialog(&call, "a-svc", DialogState::Confirmed)].into();
    call.a_leg.own_tag = Some("a-svc".into());
    call.b_legs = vec![
        confirmed("b-1", LegKind::Destination, B1),
        confirmed("b-2", LegKind::Destination, B2),
        confirmed("m-1", LegKind::Media, M1),
    ];
    call.active_peer = Some(ActivePeer { leg_a: "a".into(), leg_b: "b-1".into() });
    call.features = Some(features(option, uncharged));
    call
}

/// The re-INVITE the stack sends on `leg`, its body written by `author`
/// (`None`: the stack's own), with `add` stated on it.
fn reinvite(call: &call::Call, leg: &str, author: Option<&str>, add: Vec<Entry>) -> HandlerResult {
    let body = match author {
        Some(l) => Body::new(b"v=0\r\n".to_vec(), None, BodyAuthor::Leg(l.into())),
        None => Body::own(b"v=0\r\n".to_vec(), None),
    };
    super::execute_on(
        call,
        &[RuleAction::SendReinvite { leg_id: leg.into(), body: Some(body), add_headers: add }],
    )
}

fn vectors_sent(result: &HandlerResult, leg: &str) -> Vec<String> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some(leg))
        .find_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Request(r) => {
                Some(r.raw(HeaderName::from("P-Charging-Vector")).map(str::to_string).collect())
            }
            _ => None,
        })
        .expect("a re-INVITE on the leg")
}

#[test]
fn toward_an_originated_leg_the_vector_its_invite_carried_and_the_retained_bytes_carry_it() {
    let c = call(None, true, false);
    let result = reinvite(&c, "b-2", Some("a"), vec![]);
    assert_eq!(vectors_sent(&result, "b-2"), [B2]);
    let held = result.call.b_legs[1]
        .dialogs
        .confirmed()
        .expect("the confirmed dialog")
        .pending_invite_txn
        .as_ref()
        .expect("the re-INVITE's client transaction is held");
    let retained = String::from_utf8_lossy(&held.original_invite);
    assert!(retained.contains(B2), "the held bytes are the request as it left: {retained}");
}

#[test]
fn toward_the_originator_its_own_vector_whatever_the_body_and_none_where_it_sent_none() {
    for author in [Some("b-2"), Some("b-1"), Some("m-1"), None] {
        let own = reinvite(&call(Some(CALLER), true, false), "a", author, vec![]);
        assert_eq!(vectors_sent(&own, "a"), [CALLER], "{author:?}");
        let none = reinvite(&call(None, true, false), "a", author, vec![]);
        assert!(vectors_sent(&none, "a").is_empty(), "{author:?}: no vector is minted for it");
    }
}

#[test]
fn an_uncharged_media_leg_takes_none() {
    assert!(vectors_sent(&reinvite(&call(None, true, true), "m-1", None, vec![]), "m-1").is_empty());
    assert_eq!(vectors_sent(&reinvite(&call(None, true, false), "m-1", None, vec![]), "m-1"), [M1]);
}

#[test]
fn a_vector_the_action_states_stands_and_the_option_off_stamps_none() {
    let stated = Entry::raw(HeaderName::from("P-Charging-Vector"), STATED.to_string());
    let result = reinvite(&call(None, true, false), "b-2", Some("a"), vec![stated]);
    assert_eq!(vectors_sent(&result, "b-2"), [STATED]);
    assert!(vectors_sent(
        &reinvite(&call(Some(CALLER), false, false), "a", Some("b-2"), vec![]),
        "a"
    )
    .is_empty());
}
