//! Each model invariant holds on a sound record and names the violation on a
//! broken one.

use call::{Call, Dialog, DialogState, LegState, OutboundInvite, PeerDialog, ReliableProvisional};
use sip_message::generators::{
    generate_out_of_dialog_request, generate_response, GenerateOutOfDialogRequestOpts,
    GenerateResponseOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::SipStr;

use super::*;
use crate::config::B2buaConfig;

fn uri(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

/// A call holding the caller's INVITE and one callee leg `b-1`, both with
/// one dialog: the a-dialog under `a1`, fronting the callee's under `b1`.
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
    let mut a1 = dialog("a1", "atag");
    a1.fronts = vec![PeerDialog { leg_id: "b-1".into(), peer_tag: "b1".into() }];
    call.a_leg.dialogs = vec![a1].into();
    call.a_leg.own_tag = Some("a1".into());
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = "bcid".into();
    b.own_tag = None;
    b.outbound_invite = Some(OutboundInvite { our_tag: "own".into(), cseq: 1 });
    b.dialogs = vec![dialog("own", "b1")].into();
    call::helpers::add_b_leg(call, b)
}

fn dialog(local: &str, remote: &str) -> Dialog {
    let mut d = Dialog::new(local, remote, DialogState::Early, 1);
    d.peer_cseq = Some(1);
    d
}

fn after(call: &Call) -> Turn<'_> {
    Turn { before: None, after: call, received: None, sent: &[] }
}

#[test]
fn a_sound_call_breaks_no_invariant() {
    let call = call();
    for (name, check) in INVARIANTS {
        assert_eq!(
            check(&Turn { before: Some(&call), after: &call, received: None, sent: &[] }),
            Ok(()),
            "{name}"
        );
    }
}

#[test]
fn a_confirmed_a_leg_with_two_dialogs_breaks_the_one_dialog_invariant() {
    let mut call = call();
    call.a_leg.state = LegState::Confirmed;
    call.a_leg.dialogs.insert(dialog("a2", "atag"));
    let err = confirmed_a_leg_one_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("holds 2 live dialogs"), "{err}");
}

#[test]
fn a_front_naming_no_callee_dialog_breaks_the_fronts_invariant() {
    let mut call = call();
    call.a_leg.dialogs.by_tags_mut("a1", "atag").unwrap().fronts[0].peer_tag = "gone".into();
    let err = fronts_name_a_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("a1 fronts b-1:gone"), "{err}");
}

#[test]
fn a_remote_cseq_going_down_breaks_the_monotonic_invariant() {
    let before = call();
    let mut now = before.clone();
    now.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().peer_cseq = Some(0);
    let err = dialog_cseq_never_decreases(&Turn {
        before: Some(&before),
        after: &now,
        received: None,
        sent: &[],
    })
    .unwrap_err();
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

#[test]
fn two_dialogs_of_a_leg_under_the_same_tags_break_the_naming_invariant() {
    let mut call = call();
    call.b_legs[0].dialogs.insert(dialog("own", "b2"));
    call.b_legs[0].dialogs.by_tags_mut("own", "b2").unwrap().peer_tag = "b1".into();
    let err = dialog_named_once(&after(&call)).unwrap_err();
    assert!(err.contains("two dialogs are named own/b1"), "{err}");
}

#[test]
fn a_dialog_without_its_own_tag_breaks_the_naming_invariant() {
    let mut call = call();
    call.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().our_tag = String::new();
    let err = dialog_named_once(&after(&call)).unwrap_err();
    assert!(err.contains("has no own tag"), "{err}");
}

#[test]
fn a_confirmed_leg_without_a_confirmed_dialog_breaks_the_leg_confirmed_invariant() {
    let mut call = call();
    call.b_legs[0].state = LegState::Confirmed;
    let err = leg_confirmed_one_confirmed_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("leg b-1 is Confirmed"), "{err}");
}

#[test]
fn a_confirmed_dialog_on_an_unconfirmed_leg_breaks_the_leg_confirmed_invariant() {
    let mut call = call();
    call.b_legs[0].state = LegState::Terminated;
    call.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().state = DialogState::Ending;
    assert_eq!(
        leg_confirmed_one_confirmed_dialog(&after(&call)),
        Ok(()),
        "a released leg's BYE in flight"
    );
    call.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().state = DialogState::Confirmed;
    let err = leg_confirmed_one_confirmed_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("leg b-1 is Terminated"), "{err}");
}

#[test]
fn two_confirmed_dialogs_on_a_leg_break_the_leg_confirmed_invariant() {
    let mut call = call();
    call.b_legs[0].state = LegState::Confirmed;
    call.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().state = DialogState::Confirmed;
    let mut fork = dialog("own", "b2");
    fork.state = DialogState::Ending;
    call.b_legs[0].dialogs.insert(fork);
    let err = leg_confirmed_one_confirmed_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("leg b-1 is Confirmed"), "{err}");
}

#[test]
fn an_own_tag_naming_no_dialog_breaks_the_own_tag_invariant() {
    let mut call = call();
    call.a_leg.own_tag = Some("gone".into());
    let err = own_tag_names_a_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("own tag gone names no dialog"), "{err}");
    call.a_leg.own_tag = None;
    let err = own_tag_names_a_dialog(&after(&call)).unwrap_err();
    assert!(err.contains("leg a holds dialogs but no own tag"), "{err}");
}

#[test]
fn a_local_cseq_going_down_breaks_the_monotonic_invariant() {
    let mut before = call();
    before.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().our_cseq = 3;
    let mut now = before.clone();
    now.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().our_cseq = 2;
    let err = dialog_cseq_never_decreases(&Turn {
        before: Some(&before),
        after: &now,
        received: None,
        sent: &[],
    })
    .unwrap_err();
    assert!(err.contains("local CSeq went from 3 to 2"), "{err}");
}

#[test]
fn a_dialog_dropped_from_its_leg_breaks_the_monotonic_invariant() {
    let before = call();
    let mut now = before.clone();
    now.b_legs[0].dialogs.retain(|d| d.peer_tag != "b1");
    let err = dialog_cseq_never_decreases(&Turn {
        before: Some(&before),
        after: &now,
        received: None,
        sent: &[],
    })
    .unwrap_err();
    assert!(err.contains("dialog own/b1 is gone"), "{err}");
}

#[test]
fn an_early_dialog_past_its_invite_breaks_the_early_dialog_invariant() {
    let mut call = call();
    call.a_leg.state = LegState::Early;
    call.b_legs[0].state = LegState::Early;
    assert_eq!(no_early_dialog_past_its_invite(&after(&call)), Ok(()), "both INVITEs open");
    call.b_legs[0].state = LegState::Terminated;
    let err = no_early_dialog_past_its_invite(&after(&call)).unwrap_err();
    assert!(err.contains("leg b-1 (Terminated) holds early dialog own/b1"), "{err}");
    call.b_legs[0].state = LegState::Early;
    call.a_leg.invite_final_sent = Some(486);
    let err = no_early_dialog_past_its_invite(&after(&call)).unwrap_err();
    assert!(err.contains("leg a (answered 486) holds early dialog a1/atag"), "{err}");
}

/// The caller's in-dialog MESSAGE under her dialog `a1`, CSeq 2.
fn in_dialog_message() -> sip_message::SipRequest {
    generate_out_of_dialog_request(
        OutOfDialogMethod::Message,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri("sip:b2bua@10.0.0.9:5080")),
            call_id: "acid@alice".into(),
            from: Some(
                header::From::from_uri(uri("sip:alice@10.0.0.1"))
                    .with_tag(SipStr::from_static("atag")),
            ),
            to: Some(
                header::To::from_uri(uri("sip:bob@10.0.0.9")).with_tag(SipStr::from_static("a1")),
            ),
            cseq: 2,
            via: Some(Via::udp("10.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKm"))),
            contact: Some(header::Contact::from_uri(uri("sip:alice@10.0.0.1:5060"))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![],
        },
    )
}

fn response(req: &sip_message::SipRequest, status: u16) -> crate::effects::OutboundSipEffect {
    crate::effects::OutboundSipEffect {
        body: crate::effects::OutboundBody::Response(generate_response(
            req,
            status,
            "Reason",
            &GenerateResponseOpts::default(),
        )),
        mode: crate::effects::OutboundTxnMode::ServerResponse,
        destination: ("10.0.0.1".into(), 5060),
        label: "test".into(),
        leg_id: Some("a".into()),
        provenance: crate::effects::Provenance::Authored,
    }
}

#[test]
fn an_in_dialog_request_neither_answered_nor_relayed_breaks_the_answer_invariant() {
    let call = call();
    let req = in_dialog_message();
    let turn = |sent| Turn { before: None, after: &call, received: Some(&req), sent };
    let err = in_dialog_request_answered_or_relayed(&turn(&[])).unwrap_err();
    assert!(err.contains("MESSAGE CSeq 2 ends the turn neither answered nor relayed"), "{err}");
    let provisional = [response(&req, 183)];
    assert!(
        in_dialog_request_answered_or_relayed(&turn(&provisional)).is_err(),
        "a 1xx is no final"
    );
    let answered = [response(&req, 200)];
    assert_eq!(in_dialog_request_answered_or_relayed(&turn(&answered)), Ok(()), "answered");

    let mut relayed = call.clone();
    relayed.b_legs[0].dialogs.by_tags_mut("own", "b1").unwrap().inbound_pending_requests.push(
        call::PendingRequest {
            method: "MESSAGE".into(),
            outbound_cseq: 2,
            inbound_cseq: 2,
            source_vias: Vec::new(),
            source_call_id: "acid@alice".into(),
            source_from: String::new(),
            source_to: String::new(),
            direction: call::Direction::FromA,
            cancelled: false,
            offered: false,
            offered_100rel: false,
            source_timestamp: None,
        },
    );
    let turn = Turn { before: None, after: &relayed, received: Some(&req), sent: &[] };
    assert_eq!(in_dialog_request_answered_or_relayed(&turn), Ok(()), "relayed");
}
