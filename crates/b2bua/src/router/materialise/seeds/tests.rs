use std::net::SocketAddr;

use super::*;
use call::{
    Dialog, DialogState, Direction, HostPort, LegKind, LegState, OutboundInvite, RemoteInfo,
};
use sip_message::generators::{
    generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri};

use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;

const A_BRANCH: &str = "z9hG4bK-alice";
const B_BRANCH: &str = "z9hG4bK-b1";
const REINVITE_BRANCH: &str = "z9hG4bK-b1-reinvite";
const RELAYED_BRANCH: &str = "z9hG4bK-proxy-reinvite";

fn uri(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

/// The caller's INVITE as the a-leg snapshot records it.
fn a_invite() -> SipRequest {
    generate_out_of_dialog_request(
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
            via: Some(Via::udp("10.0.0.1", 5060).with_branch(SipStr::from_static(A_BRANCH))),
            contact: Some(header::Contact::from_uri(uri("sip:alice@10.0.0.1:5060"))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![],
        },
    )
    .expect("the caller's INVITE")
}

/// A b-leg INVITE handle on `branch` with CSeq `cseq`, stamped with this
/// call's `cr` / `lg`.
fn b_handle(branch: &str, cseq: u32) -> InviteTxnHandle {
    let invite = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri("sip:bob@10.0.0.2:5070")),
            call_id: "bcid@x".into(),
            from: Some(
                header::From::from_uri(uri("sip:svc@10.0.0.9"))
                    .with_tag(SipStr::from_static("svc")),
            ),
            to: Some(header::To::from_uri(uri("sip:bob@10.0.0.2"))),
            cseq,
            via: Some(
                Via::udp("10.0.0.9", 5080)
                    .with_branch(SipStr::owned(branch))
                    .with_param(
                        SipStr::from_static("cr"),
                        header::ParamValue::Token(SipStr::from_static("w0%7Cacid%40alice%7Catag")),
                    )
                    .with_param(
                        SipStr::from_static("lg"),
                        header::ParamValue::Token(SipStr::from_static("b-1")),
                    ),
            ),
            contact: Some(header::Contact::from_uri(uri("sip:svc@10.0.0.9:5080"))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![],
        },
    )
    .expect("a b-leg INVITE");
    InviteTxnHandle {
        branch: branch.into(),
        cseq: i64::from(cseq),
        original_invite: invite.image().to_vec(),
        destination: HostPort { host: "10.0.0.2".into(), port: 5070 },
    }
}

/// The b-leg in `state`: a Trying leg holds no dialog, an Early one the
/// fork that rang, a Confirmed one the dialog its 2xx confirmed.
fn b_leg(state: LegState, handle: Option<InviteTxnHandle>) -> Leg {
    let mut leg = Leg {
        leg_id: "b-1".into(),
        call_id: "bcid@x".into(),
        source: RemoteInfo { address: "10.0.0.2".into(), port: 5070 },
        dialogs: Default::default(),
        local_uri: Some("sip:svc@10.0.0.9".into()),
        remote_uri: Some("sip:bob@10.0.0.2".into()),
        invite_request_uri: Some("sip:bob@10.0.0.2:5070".into()),
        outbound_invite: Some(OutboundInvite::new("svc", 1, 0, handle.clone())),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::Destination),
        adopted: None,
        invite_final_sent: None,
        answer_preempted: None,
        attempt: None,
        ring: call::Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: Default::default(),
    };
    let dialog_state = match state {
        LegState::Confirmed => DialogState::Confirmed,
        LegState::Early => DialogState::Early,
        LegState::Trying => return leg,
        LegState::Terminated => {
            call::testkit::stage_leg(&mut leg, state);
            return leg;
        }
    };
    let mut dialog = Dialog::new("svc", "btag", dialog_state, 1);
    dialog.remote_target = "sip:bob@10.0.0.2:5070".into();
    dialog.ack_branch = (state == LegState::Confirmed).then(|| "z9hG4bK-ack".to_string());
    dialog.pending_invite_txn = handle;
    leg.dialogs.insert(dialog);
    if state == LegState::Confirmed {
        call::testkit::stage_leg(&mut leg, state);
    }
    leg
}

/// The caller's relayed re-INVITE, pending on the b-leg dialog under
/// outbound CSeq `outbound_cseq`.
fn relayed_reinvite(outbound_cseq: i64, cancelled: bool) -> PendingRequest {
    PendingRequest {
        method: "INVITE".into(),
        outbound_cseq,
        inbound_cseq: 2,
        source_vias: vec![
            format!("SIP/2.0/UDP 10.0.0.5:5080;branch={RELAYED_BRANCH}"),
            format!("SIP/2.0/UDP 10.0.0.1:5060;branch={A_BRANCH}-2"),
        ],
        source_call_id: "acid@alice".into(),
        source_from: "<sip:alice@10.0.0.1>;tag=atag".into(),
        source_to: "<sip:bob@10.0.0.9>;tag=svca".into(),
        direction: Direction::FromA,
        cancelled,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
        relayed_by: String::new(),
    }
}

fn call_with(a_state: LegState, b: Leg) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &a_invite(),
        "10.0.0.1:5060".parse().unwrap(),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    );
    call.a_leg.invite_final_sent = match a_state {
        LegState::Trying | LegState::Early => None,
        LegState::Confirmed => Some(200),
        LegState::Terminated => Some(487),
    };
    call = call::helpers::add_b_leg(call, b);
    call.active_peer = Some(call::ActivePeer { leg_a: "a".into(), leg_b: "b-1".into() });
    call
}

/// The a-leg dialog the first provisional to the caller pinned under `tag`.
fn ring_a_leg(call: &mut Call, tag: &str) {
    let mut dialog = Dialog::new(tag, "atag", DialogState::Early, 1);
    dialog.remote_target = "sip:alice@10.0.0.1:5060".into();
    call.a_leg.dialogs.insert(dialog);
    call.a_leg.own_tag = Some(tag.into());
}

/// The b-leg's confirmed dialog, to update.
fn confirmed(leg: &mut Leg) -> &mut Dialog {
    leg.dialogs.confirmed_mut().expect("the confirmed dialog")
}

/// The branches of the server seeds still awaiting their final.
fn server_branches(seeds: &[TxnSeed]) -> Vec<String> {
    seeds
        .iter()
        .filter_map(|s| match s {
            TxnSeed::ServerInvite { request, .. } => {
                Some(request.top_via().branch().unwrap_or_default().to_string())
            }
            _ => None,
        })
        .collect()
}

/// The branches of the server seeds Accepted on a retained 2xx.
fn accepted_branches(seeds: &[TxnSeed]) -> Vec<String> {
    seeds
        .iter()
        .filter_map(|s| match s {
            TxnSeed::ServerAccepted { response, .. } => {
                Some(response.top_via().branch().unwrap_or_default().to_string())
            }
            _ => None,
        })
        .collect()
}

/// The identity a server seed is matched and answered on: Call-ID,
/// From-tag, To-tag, leg.
fn server_identity(seed: &TxnSeed) -> Option<(&str, &str, Option<&str>, Option<&str>)> {
    match seed {
        TxnSeed::ServerInvite { request, to_tag, leg_id } => Some((
            request.call_id().as_str(),
            request.from().tag().unwrap_or_default(),
            to_tag.as_deref(),
            leg_id.as_deref(),
        )),
        _ => None,
    }
}

fn client_branches(seeds: &[TxnSeed]) -> Vec<String> {
    seeds
        .iter()
        .filter_map(|s| match s {
            TxnSeed::ClientInvite { invite, .. } => {
                Some(invite.top_via().branch().unwrap_or_default().to_string())
            }
            _ => None,
        })
        .collect()
}

/// A ringing call: the a-leg INVITE is seeded whole as a server INVITE under
/// the To-tag the 180 pinned, and the b-leg INVITE once as a client INVITE,
/// though the handle sits on the leg and on its dialog. Nothing names the
/// a-leg branch for an in-dialog re-offer.
#[test]
fn a_ringing_call_seeds_the_a_leg_server_invite_and_the_b_leg_client_invite_once() {
    let mut call = call_with(LegState::Early, b_leg(LegState::Early, Some(b_handle(B_BRANCH, 1))));
    ring_a_leg(&mut call, "svca");
    assert!(
        !names_server_branch(&call, A_BRANCH),
        "the caller's INVITE is no in-dialog re-offer target"
    );
    assert!(!names_server_branch(&call, B_BRANCH));
    let seeds = seeds_for(&call);
    assert_eq!(seeds.len(), 2, "{seeds:?}");
    assert_eq!(server_branches(&seeds), vec![A_BRANCH.to_string()]);
    assert_eq!(client_branches(&seeds), vec![B_BRANCH.to_string()]);
    assert_eq!(
        server_identity(&seeds[0]),
        Some(("acid@alice", "atag", Some("svca"), Some("a"))),
        "the a-leg seed first"
    );
    let TxnSeed::ClientInvite { dest, .. } = &seeds[1] else { panic!("the b-leg seed second") };
    assert_eq!(
        *dest,
        "10.0.0.2:5070".parse::<SocketAddr>().unwrap(),
        "the handle's wire destination"
    );
}

/// A call still `Trying` — nothing above a 100 went to the caller — seeds the
/// a-leg INVITE with no To-tag: the layer mints one on its first response.
#[test]
fn an_unrung_a_leg_seeds_no_to_tag() {
    let call = call_with(LegState::Trying, b_leg(LegState::Trying, Some(b_handle(B_BRANCH, 1))));
    let seeds = seeds_for(&call);
    assert_eq!(server_identity(&seeds[0]), Some(("acid@alice", "atag", None, Some("a"))));
}

/// An established call names no in-flight INVITE: the answered handles stay
/// on the record for the ACK, and seed nothing.
#[test]
fn an_established_call_seeds_nothing() {
    let call =
        call_with(LegState::Confirmed, b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1))));
    assert!(seeds_for(&call).is_empty());
}

/// A relayed re-INVITE in flight seeds both halves of the round: the
/// caller's INVITE as a server seed on the proxy's branch, rebuilt from the
/// snapshot as far as a response reads it, and the re-INVITE toward the
/// callee as a client seed.
#[test]
fn a_relayed_reinvite_in_flight_seeds_both_halves_of_the_round() {
    let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
    confirmed(&mut leg).pending_invite_txn = Some(b_handle(REINVITE_BRANCH, 2));
    confirmed(&mut leg).ack_branch = None;
    let mut pending = relayed_reinvite(2, false);
    pending.inbound_cseq = 7;
    pending.source_timestamp = Some("54.2".into());
    confirmed(&mut leg).inbound_pending_requests.push(pending);
    let call = call_with(LegState::Confirmed, leg);
    let seeds = seeds_for(&call);
    assert_eq!(server_branches(&seeds), vec![RELAYED_BRANCH.to_string()]);
    let request = seeds
        .iter()
        .find_map(|s| match s {
            TxnSeed::ServerInvite { request, .. } => Some(request),
            _ => None,
        })
        .expect("the server seed");
    assert_eq!(request.method(), "INVITE");
    assert_eq!(request.cseq().seq(), 7, "the originator's own CSeq");
    assert_eq!(
        request.raw(header::HeaderName::Via).collect::<Vec<_>>(),
        [
            format!("SIP/2.0/UDP 10.0.0.5:5080;branch={RELAYED_BRANCH}"),
            format!("SIP/2.0/UDP 10.0.0.1:5060;branch={A_BRANCH}-2"),
        ],
        "the Via stack it arrived with, in order"
    );
    assert_eq!(request.raw(header::HeaderName::Timestamp).collect::<Vec<_>>(), ["54.2"]);
    assert_eq!(request.request_uri().to_string(), "sip:bob@10.0.0.9", "the To URI");
    assert_eq!(client_branches(&seeds), vec![REINVITE_BRANCH.to_string()]);
    let server = seeds.iter().find_map(server_identity);
    assert_eq!(
        server,
        Some(("acid@alice", "atag", Some("svca"), Some("a"))),
        "the originator's identity, its To-tag and leg"
    );
    assert!(
        names_server_branch(&call, RELAYED_BRANCH),
        "the relayed INVITE's branch is a server seed"
    );
    assert!(!names_server_branch(&call, REINVITE_BRANCH), "the client half is no server seed");
    assert!(!names_server_branch(&call, A_BRANCH), "an answered a-leg names no server INVITE");
}

/// A re-INVITE this stack originated itself (no relayed snapshot) is in
/// flight until its final: seeded as a client INVITE so its late final is
/// ACKed and bounded here. Once a 2xx is taken — the ACK still owed — or a
/// non-2xx final closed the round on the record, nothing is in flight.
#[test]
fn a_self_originated_reinvite_in_flight_seeds_its_client_invite() {
    let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
    confirmed(&mut leg).pending_invite_txn = Some(b_handle(REINVITE_BRANCH, 2));
    confirmed(&mut leg).ack_branch = None;
    let open = call_with(LegState::Confirmed, leg.clone());
    let seeds = seeds_for(&open);
    assert!(server_branches(&seeds).is_empty(), "nothing was relayed, so no server seed");
    assert_eq!(client_branches(&seeds), vec![REINVITE_BRANCH.to_string()]);

    let mut answered = leg.clone();
    confirmed(&mut answered).awaited_ack_cseq = Some(2);
    assert!(
        seeds_for(&call_with(LegState::Confirmed, answered)).is_empty(),
        "a 2xx taken closes the round"
    );

    let rejected = call::helpers::close_rejected_invite_round(open, "b-1", REINVITE_BRANCH);
    assert!(seeds_for(&rejected).is_empty(), "a non-2xx final closed the round on the record");
}

/// The initial INVITE's handle is closed by nothing but the leg's state: a
/// non-2xx final on a still-early leg leaves it for the CANCEL path.
#[test]
fn closing_a_round_touches_only_a_confirmed_leg() {
    let call = call_with(LegState::Early, b_leg(LegState::Early, Some(b_handle(B_BRANCH, 1))));
    let call = call::helpers::close_rejected_invite_round(call, "b-1", B_BRANCH);
    let fork = call.b_legs[0].dialogs.by_tags("svc", "btag").expect("the fork that rang");
    assert!(fork.pending_invite_txn.is_some());
}

/// A relayed re-INVITE already CANCELled has had its finals from the node
/// that held it: neither half is in flight.
#[test]
fn a_cancelled_relayed_reinvite_seeds_nothing() {
    let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
    confirmed(&mut leg).pending_invite_txn = Some(b_handle(REINVITE_BRANCH, 2));
    confirmed(&mut leg).ack_branch = None;
    confirmed(&mut leg).inbound_pending_requests.push(relayed_reinvite(2, true));
    assert!(seeds_for(&call_with(LegState::Confirmed, leg)).is_empty());
}

/// A b-leg whose handle does not read seeds only what does.
#[test]
fn an_unreadable_handle_is_skipped() {
    let mut handle = b_handle(B_BRANCH, 1);
    handle.original_invite = b"garbage".to_vec();
    let call = call_with(LegState::Early, b_leg(LegState::Trying, Some(handle)));
    let seeds = seeds_for(&call);
    assert_eq!(server_branches(&seeds), vec![A_BRANCH.to_string()]);
    assert!(client_branches(&seeds).is_empty());
}

/// A 2xx `leg`'s confirmed dialog retains for a re-INVITE whose top Via
/// carries `branch`.
fn retain_reinvite_2xx(leg: &mut Leg, branch: &str) {
    let ok = format!(
        "SIP/2.0 200 OK\r\n\
         Via: SIP/2.0/UDP 10.0.0.5:5080;branch={branch}\r\n\
         From: <sip:bob@10.0.0.2>;tag=btag\r\n\
         To: <sip:svc@10.0.0.9>;tag=svc\r\n\
         Call-ID: bcid@x\r\n\
         CSeq: 4 INVITE\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let (emission, _) = call::RetainedEmission::paced(
        ok.into_bytes(),
        ("10.0.0.5".into(), 5080),
        sip_retransmit::Class::Final2xx,
        call::Repeated::response("INVITE", 200),
    );
    confirmed(leg).pending_reinvite_2xx =
        Some(Unacked2xx { dialog_tag: "svc".into(), cseq: 4, emission });
}

/// A re-INVITE whose 2xx the dialog retains awaiting its ACK seeds an
/// Accepted server INVITE on the 2xx's branch, attributed to the dialog's
/// leg; that branch is a server seed an in-dialog INVITE can be a copy of,
/// and the call has an INVITE transaction a live copy must serve: the 2xx's
/// retransmission ladder.
#[test]
fn a_retained_reinvite_2xx_seeds_an_accepted_server_invite() {
    let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
    retain_reinvite_2xx(&mut leg, RELAYED_BRANCH);
    let call = call_with(LegState::Confirmed, leg);
    let seeds = seeds_for(&call);
    assert_eq!(accepted_branches(&seeds), vec![RELAYED_BRANCH.to_string()], "{seeds:?}");
    let leg_id = seeds.iter().find_map(|s| match s {
        TxnSeed::ServerAccepted { leg_id, .. } => leg_id.as_deref(),
        _ => None,
    });
    assert_eq!(leg_id, Some("b-1"), "the originator's leg holds the dialog");
    assert!(names_server_branch(&call, RELAYED_BRANCH), "a copy of its INVITE is re-offered");
    assert!(super::super::has_invite_to_serve(&call), "its 2xx awaits the ACK");
}

/// The b-leg client seed's `provisional` flag and the `Reason` lines of the
/// CANCEL it carries (`None`: no CANCEL).
fn client_cancel(seeds: &[TxnSeed]) -> (bool, Option<Vec<String>>) {
    seeds
        .iter()
        .find_map(|s| match s {
            TxnSeed::ClientInvite { provisional, cancel, .. } => Some((
                *provisional,
                cancel.as_ref().map(|c| {
                    assert_eq!(c.method(), "CANCEL");
                    c.raw(header::HeaderName::Reason).map(str::to_string).collect()
                }),
            )),
            _ => None,
        })
        .expect("a client seed")
}

/// A b-leg INVITE with no dialog on record drew no provisional the record
/// knows of: its seed opens `Trying`, so a CANCEL on it waits (RFC 3261 §9.1).
#[test]
fn a_b_leg_with_no_dialog_seeds_its_invite_with_no_provisional() {
    let call = call_with(LegState::Trying, b_leg(LegState::Trying, Some(b_handle(B_BRANCH, 1))));
    assert_eq!(client_cancel(&seeds_for(&call)), (false, None));
}

/// An early dialog is the record of a provisional.
#[test]
fn an_early_b_leg_seeds_its_invite_with_a_provisional() {
    let call = call_with(LegState::Early, b_leg(LegState::Early, Some(b_handle(B_BRANCH, 1))));
    assert_eq!(client_cancel(&seeds_for(&call)), (true, None));
}

/// A CANCEL the record says was sent with no provisional on record is still
/// held: it rides the seed, stating the `Reason` it stated. One sent after a
/// provisional is on the wire already; the early dialog it is ending still
/// records that provisional.
#[test]
fn a_cancelled_b_leg_seeds_its_held_cancel_with_the_stated_reason() {
    let reason = vec![("Reason".to_string(), "Q.850;cause=16".to_string())];
    let call = call_with(LegState::Trying, b_leg(LegState::Trying, Some(b_handle(B_BRANCH, 1))));
    let call = call::helpers::record_cancel_sent(call, "b-1", reason.clone());
    assert_eq!(client_cancel(&seeds_for(&call)), (false, Some(vec!["Q.850;cause=16".to_string()])));

    let call = call_with(LegState::Early, b_leg(LegState::Early, Some(b_handle(B_BRANCH, 1))));
    let call = call::helpers::record_cancel_sent(call, "b-1", reason);
    assert_eq!(client_cancel(&seeds_for(&call)), (true, None));
}
