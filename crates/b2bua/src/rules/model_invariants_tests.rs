//! Each model invariant holds on a sound record and names the violation on a
//! broken one.

use call::helpers::{make_dialog_from_incoming, MakeDialogLegCtx};
use call::{Call, Dialog, LegState, ReliableProvisional, TagMapping};
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::SipStr;

use super::*;
use crate::config::B2buaConfig;

fn uri(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

/// A call holding the caller's INVITE and one callee leg `b-1`, both with
/// one dialog: the a-dialog under `a1`, the callee's under `b1`.
fn call() -> Call {
    let invite = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri("sip:bob@10.0.0.9:5080")),
            call_id: "acid@alice".into(),
            from: Some(
                header::From::from_uri(uri("sip:alice@10.0.0.1"))
                    .with_tag(SipStr::from_static("atag")),
            ),
            to: Some(header::To::from_uri(uri("sip:bob@10.0.0.9"))),
            cseq: 1,
            via: Some(Via::udp("10.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKa"))),
            contact: Some(header::Contact::from_uri(uri("sip:alice@10.0.0.1:5060"))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![],
        },
    );
    let mut call = crate::initial_invite::build_initial_call(
        &invite,
        "10.0.0.1:5060".parse().unwrap(),
        &B2buaConfig::default(),
        &sip_txn::IdGen::seeded(1),
        0,
    );
    call.a_leg.dialogs = vec![dialog("acid@alice", "a1", "atag")];
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = "bcid".into();
    b.dialogs = vec![dialog("bcid", "own", "b1")];
    call = call::helpers::add_b_leg(call, b);
    call.tag_map.push(TagMapping {
        a_tag: "a1".into(),
        b_leg_id: "b-1".into(),
        b_tag: "b1".into(),
    });
    call
}

fn dialog(call_id: &str, local: &str, remote: &str) -> Dialog {
    let ctx = MakeDialogLegCtx {
        call_id,
        local_uri: "sip:l@h",
        remote_uri: "sip:r@h",
        local_tag: local,
        remote_tag: remote,
    };
    make_dialog_from_incoming(&ctx, 1, vec![], 1)
}

fn after(call: &Call) -> Turn<'_> {
    Turn { before: None, after: call }
}

#[test]
fn a_sound_call_breaks_no_invariant() {
    let call = call();
    for (name, check) in INVARIANTS {
        assert_eq!(check(&Turn { before: Some(&call), after: &call }), Ok(()), "{name}");
    }
}

#[test]
fn a_confirmed_a_leg_with_two_dialogs_breaks_the_one_dialog_invariant() {
    let mut call = call();
    call.a_leg.state = LegState::Confirmed;
    call.a_leg.dialogs.push(dialog("acid@alice", "a2", "atag"));
    let err = confirmed_a_leg_one_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("holds 2 dialogs"), "{err}");
}

#[test]
fn a_mapping_naming_no_a_dialog_breaks_the_tag_map_invariant() {
    let mut call = call();
    call.tag_map[0].a_tag = "gone".into();
    let err = tag_map_names_a_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("gone -> b-1:b1"), "{err}");
}

#[test]
fn a_remote_cseq_going_down_breaks_the_monotonic_invariant() {
    let before = call();
    let mut now = before.clone();
    now.b_legs[0].dialogs[0].ext.remote_cseq = Some(0);
    let err =
        dialog_cseq_never_decreases(&Turn { before: Some(&before), after: &now }).unwrap_err();
    assert!(err.contains("remote CSeq went from 1"), "{err}");
}

#[test]
fn an_unacknowledged_reliable_1xx_on_an_ended_leg_breaks_the_prack_invariant() {
    let mut call = call();
    call.reliable_provisionals.push(ReliableProvisional {
        a_tag: "a1".into(),
        a_rseq: 1,
        b_leg_id: "b-1".into(),
        b_tag: "b1".into(),
        b_cseq: 1,
        b_rseq: 7,
        acknowledged: false,
        emission: None,
        a_cseq: 1,
        carried_sdp: false,
        responder_sdp: false,
        responder_offer: None,
    });
    call.b_legs[0].state = LegState::Early;
    assert_eq!(reliable_1xx_acknowledged_at_final(&after(&call)), Ok(()), "INVITE still open");
    call.b_legs[0].state = LegState::Terminated;
    let err = reliable_1xx_acknowledged_at_final(&after(&call)).unwrap_err();
    assert!(err.contains("RSeq 7"), "{err}");
}

#[test]
fn an_early_a_leg_past_a_rejection_breaks_the_ended_dialog_invariant() {
    let mut call = call();
    call.a_leg.state = LegState::Early;
    call.a_leg.invite_final_sent = Some(486);
    let err = no_a_dialog_past_a_rejection(&after(&call)).unwrap_err();
    assert!(err.contains("took 486"), "{err}");
}
