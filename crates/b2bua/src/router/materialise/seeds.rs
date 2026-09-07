//! The seeds a materialised call hands the transaction layer: the in-flight
//! INVITE transactions the record names, read off the `Call` alone. Pure — the
//! layer takes typed seeds and never learns the record (ADR-0014).
//!
//! In flight, on the record's own terms:
//! - a **client INVITE** this stack sent: the initial INVITE of a leg still
//!   `Trying` / `Early` (its handle on the leg and on every early dialog), and
//!   a re-INVITE whose round is still open on its confirmed dialog, whichever
//!   side originated it. A handle names an open round while no 2xx has been
//!   taken for it (`ack_branch` / `awaited_ack_cseq` unset — the handle stays
//!   after a 2xx for the ACK and its re-ACKs) and the record has not closed it
//!   (a non-2xx final drops it, `close_rejected_invite_round`); a relayed round
//!   already CANCELled toward its target awaits a final only the dead node's
//!   Via can carry, so it is not in flight here.
//! - a **server INVITE** this stack admitted: the a-leg's initial INVITE while
//!   the a-leg is unanswered, rebuilt whole so a CANCEL draws its 487 under the
//!   To-tag the caller's early dialog holds, and every relayed INVITE still
//!   pending on a dialog, keyed by the originator's top Via with no request
//!   (only the retransmission is absorbed). A pending request already
//!   CANCELled has had its 200 + 487 from the node that held it and is not in
//!   flight.

use std::collections::HashSet;
use std::net::SocketAddr;

use call::{Call, Dialog, InviteTxnHandle, Leg, LegState, PendingRequest};
use sip_message::header::{self, HeaderValue, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipStr};
use sip_txn::TxnSeed;

use crate::rules::relay::rebuild_a_leg_invite;

/// Every in-flight INVITE transaction `call` names, one seed per branch.
pub(super) fn seeds_for(call: &Call) -> Vec<TxnSeed> {
    let mut seeds = Vec::new();
    let mut branches = HashSet::new();
    let mut push = |seed: TxnSeed| {
        let branch = match &seed {
            TxnSeed::ClientInvite { invite, .. } => invite.top_via().branch().unwrap_or_default().to_string(),
            TxnSeed::ServerInvite { branch, .. } => branch.clone(),
        };
        if !branch.is_empty() && branches.insert(branch) {
            seeds.push(seed);
        }
    };

    if matches!(call.a_leg.state, LegState::Trying | LegState::Early) {
        let invite = rebuild_a_leg_invite(&call.a_leg_invite);
        push(TxnSeed::ServerInvite {
            branch: invite.top_via().branch().unwrap_or_default().to_string(),
            call_id: invite.call_id().as_str().to_string(),
            from_tag: invite.from().tag().unwrap_or_default().to_string(),
            // The tag every provisional to the caller carried (`ensure_a_dialog`);
            // none while nothing above a 100 went out.
            to_tag: call.a_leg.dialogs.first().map(|d| d.sip.local_tag.clone()).filter(|t| !t.is_empty()),
            leg_id: Some(call.a_leg.leg_id.clone()),
            original_request: Some(invite),
        });
    }

    for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
        let initial_in_flight = matches!(leg.state, LegState::Trying | LegState::Early);
        if initial_in_flight {
            if let Some(seed) = leg.pending_invite_txn.as_ref().and_then(client_seed) {
                push(seed);
            }
        }
        for dialog in &leg.dialogs {
            if let Some(handle) = &dialog.ext.pending_invite_txn {
                if initial_in_flight || round_open(dialog, handle) {
                    if let Some(seed) = client_seed(handle) {
                        push(seed);
                    }
                }
            }
            for pending in dialog.ext.inbound_pending_requests.iter().filter(|p| pending_invite(p)) {
                if let Some(seed) = relayed_server_seed(call, leg, pending) {
                    push(seed);
                }
            }
        }
    }
    seeds
}

/// Whether `branch` is a relayed INVITE still pending on one of `call`'s
/// dialogs — the one server seed an in-dialog INVITE can name. The a-leg's
/// initial INVITE is not asked about: its retransmission carries no To-tag,
/// resolves as an initial INVITE and never reaches the in-dialog re-offer; the
/// layer's own seed absorbs it below the router.
pub(super) fn names_server_branch(call: &Call, branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| leg.dialogs.iter())
        .flat_map(|d| d.ext.inbound_pending_requests.iter())
        .filter(|p| pending_invite(p))
        .any(|p| pending_branch(p).as_deref() == Some(branch))
}

/// A re-INVITE round on a confirmed dialog still open for `handle`'s INVITE:
/// no 2xx taken for it, and — for a relayed round — its snapshot not already
/// CANCELled toward the target.
fn round_open(dialog: &Dialog, handle: &InviteTxnHandle) -> bool {
    if dialog.ext.ack_branch.is_some() || dialog.ext.awaited_ack_cseq.is_some() {
        return false;
    }
    let Some(cseq) = parse_request(&handle.original_invite).map(|r| r.cseq().seq() as i64) else {
        return false;
    };
    dialog
        .ext
        .inbound_pending_requests
        .iter()
        .filter(|p| p.method.eq_ignore_ascii_case("INVITE") && p.outbound_cseq == cseq)
        .all(|p| !p.cancelled)
}

/// The originator's top-Via branch a pending relayed request was received on.
fn pending_branch(p: &PendingRequest) -> Option<String> {
    let top_via = p.source_vias.first()?;
    Via::parse(&SipStr::owned(top_via)).ok()?.branch().map(str::to_string)
}

fn pending_invite(p: &PendingRequest) -> bool {
    p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled
}

/// The client seed for an INVITE handle: the retained INVITE and the wire
/// destination it was sent to. `None` for a handle whose bytes or destination
/// do not read — nothing can be rebuilt from it.
fn client_seed(handle: &InviteTxnHandle) -> Option<TxnSeed> {
    let invite = parse_request(&handle.original_invite)?;
    let dest: SocketAddr = format!("{}:{}", handle.destination.host, handle.destination.port).parse().ok()?;
    Some(TxnSeed::ClientInvite { invite, dest })
}

/// The server seed for a relayed INVITE pending on one of `leg`'s dialogs:
/// keyed by the originator's top-Via branch, matched on the originator's own
/// Call-ID and From-tag, answered under the To-tag its To already carries,
/// attributed to the originator's leg. The snapshot holds no request, so only
/// the INVITE's retransmissions are absorbed.
fn relayed_server_seed(call: &Call, leg: &Leg, pending: &PendingRequest) -> Option<TxnSeed> {
    let branch = pending_branch(pending)?;
    let from_tag = header::From::parse(&SipStr::owned(&pending.source_from))
        .ok()?
        .tag()
        .unwrap_or_default()
        .to_string();
    let to_tag = header::To::parse(&SipStr::owned(&pending.source_to))
        .ok()
        .and_then(|to| to.tag().map(str::to_string));
    let originator = call::helpers::get_peer(call, &leg.leg_id)
        .map(str::to_string)
        .unwrap_or_else(|| call.a_leg.leg_id.clone());
    Some(TxnSeed::ServerInvite {
        branch,
        call_id: pending.source_call_id.clone(),
        from_tag,
        to_tag,
        leg_id: Some(originator),
        original_request: None,
    })
}

/// A request re-parsed from the bytes an INVITE handle caches it as.
fn parse_request(bytes: &[u8]) -> Option<SipRequest> {
    match CustomParser::new().parse(bytes).ok()? {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use call::{B2buaDialogExt, Dialog, Direction, HostPort, LegDisposition, LegKind, RemoteInfo, StackDialog};
    use sip_message::generators::{generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts, OutOfDialogMethod};
    use sip_message::header::Uri;

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
                from: Some(header::From::from_uri(uri("sip:alice@10.0.0.1")).with_tag(SipStr::from_static("atag"))),
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
    }

    /// A b-leg INVITE handle on `branch` with CSeq `cseq`, stamped with this
    /// call's `cr` / `lg`.
    fn b_handle(branch: &str, cseq: u32) -> InviteTxnHandle {
        let invite = generate_out_of_dialog_request(
            OutOfDialogMethod::Invite,
            &GenerateOutOfDialogRequestOpts {
                request_uri: Some(uri("sip:bob@10.0.0.2:5070")),
                call_id: "bcid@x".into(),
                from: Some(header::From::from_uri(uri("sip:svc@10.0.0.9")).with_tag(SipStr::from_static("svc"))),
                to: Some(header::To::from_uri(uri("sip:bob@10.0.0.2"))),
                cseq,
                via: Some(
                    Via::udp("10.0.0.9", 5080)
                        .with_branch(SipStr::owned(branch))
                        .with_param(SipStr::from_static("cr"), header::ParamValue::Token(SipStr::from_static("w0%7Cacid%40alice%7Catag")))
                        .with_param(SipStr::from_static("lg"), header::ParamValue::Token(SipStr::from_static("b-1"))),
                ),
                contact: Some(header::Contact::from_uri(uri("sip:svc@10.0.0.9:5080"))),
                max_forwards: Some(70),
                body: Vec::new(),
                content_type: None,
                extra_headers: vec![],
            },
        );
        InviteTxnHandle {
            branch: branch.into(),
            original_invite: invite.image().to_vec(),
            destination: HostPort { host: "10.0.0.2".into(), port: 5070 },
        }
    }

    fn b_leg(state: LegState, handle: Option<InviteTxnHandle>) -> Leg {
        let dialog = Dialog {
            sip: StackDialog {
                call_id: "bcid@x".into(),
                local_tag: "svc".into(),
                remote_tag: if state == LegState::Confirmed { "btag".into() } else { String::new() },
                local_uri: "sip:svc@10.0.0.9".into(),
                remote_uri: "sip:bob@10.0.0.2".into(),
                remote_target: "sip:bob@10.0.0.2:5070".into(),
                local_cseq: 1,
                route_set: vec![],
            },
            ext: B2buaDialogExt {
                remote_cseq: None,
                inbound_pending_requests: vec![],
                ack_branch: (state == LegState::Confirmed).then(|| "z9hG4bK-ack".to_string()),
                pending_invite_txn: handle.clone(),
                cached_sdp: None,
                pending_reinvite_2xx: None,
                answered_2xx: None,
                emitted_ack: None,
                awaited_ack_cseq: None,
            },
        };
        Leg {
            leg_id: "b-1".into(),
            call_id: "bcid@x".into(),
            from_tag: "svc".into(),
            source: RemoteInfo { address: "10.0.0.2".into(), port: 5070 },
            state,
            disposition: LegDisposition::Pending,
            dialogs: vec![dialog],
            no_answer_timeout_sec: None,
            bye_disposition: None,
            local_uri: Some("sip:svc@10.0.0.9".into()),
            remote_uri: Some("sip:bob@10.0.0.2".into()),
            invite_request_uri: Some("sip:bob@10.0.0.2:5070".into()),
            pending_invite_txn: handle,
            ext: None,
            kind: Some(LegKind::Destination),
            adopted: None,
        }
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
            source_timestamp: None,
            direction: Direction::FromA,
            cancelled,
            offered_100rel: false,
        }
    }

    fn call_with(a_state: LegState, b: Leg) -> Call {
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut call = build_initial_call(&a_invite(), "10.0.0.1:5060".parse().unwrap(), &config, 0);
        call.a_leg.state = a_state;
        call = call::helpers::add_b_leg(call, b);
        call.active_peer = Some(call::ActivePeer { leg_a: "a".into(), leg_b: "b-1".into() });
        call
    }

    /// The a-leg dialog the first provisional to the caller pinned under `tag`.
    fn ring_a_leg(call: &mut Call, tag: &str) {
        call.a_leg.dialogs.push(Dialog {
            sip: StackDialog {
                call_id: "acid@alice".into(),
                local_tag: tag.into(),
                remote_tag: "atag".into(),
                local_uri: "sip:bob@10.0.0.9".into(),
                remote_uri: "sip:alice@10.0.0.1".into(),
                remote_target: "sip:alice@10.0.0.1:5060".into(),
                local_cseq: 1,
                route_set: vec![],
            },
            ext: B2buaDialogExt {
                remote_cseq: None,
                inbound_pending_requests: vec![],
                ack_branch: None,
                pending_invite_txn: None,
                cached_sdp: None,
                pending_reinvite_2xx: None,
                answered_2xx: None,
                emitted_ack: None,
                awaited_ack_cseq: None,
            },
        });
    }

    fn server_branches(seeds: &[TxnSeed]) -> Vec<(String, bool)> {
        seeds
            .iter()
            .filter_map(|s| match s {
                TxnSeed::ServerInvite { branch, original_request, .. } => Some((branch.clone(), original_request.is_some())),
                TxnSeed::ClientInvite { .. } => None,
            })
            .collect()
    }

    /// The identity a server seed is matched and answered on: Call-ID,
    /// From-tag, To-tag, leg.
    fn server_identity(seed: &TxnSeed) -> Option<(&str, &str, Option<&str>, Option<&str>)> {
        match seed {
            TxnSeed::ServerInvite { call_id, from_tag, to_tag, leg_id, .. } => {
                Some((call_id.as_str(), from_tag.as_str(), to_tag.as_deref(), leg_id.as_deref()))
            }
            TxnSeed::ClientInvite { .. } => None,
        }
    }

    fn client_branches(seeds: &[TxnSeed]) -> Vec<String> {
        seeds
            .iter()
            .filter_map(|s| match s {
                TxnSeed::ClientInvite { invite, .. } => Some(invite.top_via().branch().unwrap_or_default().to_string()),
                TxnSeed::ServerInvite { .. } => None,
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
        assert!(!names_server_branch(&call, A_BRANCH), "the caller's INVITE is no in-dialog re-offer target");
        assert!(!names_server_branch(&call, B_BRANCH));
        let seeds = seeds_for(&call);
        assert_eq!(seeds.len(), 2, "{seeds:?}");
        assert_eq!(server_branches(&seeds), vec![(A_BRANCH.to_string(), true)]);
        assert_eq!(client_branches(&seeds), vec![B_BRANCH.to_string()]);
        assert_eq!(server_identity(&seeds[0]), Some(("acid@alice", "atag", Some("svca"), Some("a"))), "the a-leg seed first");
        let TxnSeed::ClientInvite { dest, .. } = &seeds[1] else { panic!("the b-leg seed second") };
        assert_eq!(*dest, "10.0.0.2:5070".parse::<SocketAddr>().unwrap(), "the handle's wire destination");
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
        let call = call_with(LegState::Confirmed, b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1))));
        assert!(seeds_for(&call).is_empty());
    }

    /// A relayed re-INVITE in flight seeds both halves of the round: the
    /// caller's INVITE as a bare server seed on the proxy's branch, and the
    /// re-INVITE toward the callee as a client seed.
    #[test]
    fn a_relayed_reinvite_in_flight_seeds_both_halves_of_the_round() {
        let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
        leg.dialogs[0].ext.pending_invite_txn = Some(b_handle(REINVITE_BRANCH, 2));
        leg.dialogs[0].ext.ack_branch = None;
        leg.dialogs[0].ext.inbound_pending_requests.push(relayed_reinvite(2, false));
        let call = call_with(LegState::Confirmed, leg);
        let seeds = seeds_for(&call);
        assert_eq!(server_branches(&seeds), vec![(RELAYED_BRANCH.to_string(), false)]);
        assert_eq!(client_branches(&seeds), vec![REINVITE_BRANCH.to_string()]);
        let server = seeds.iter().find_map(server_identity);
        assert_eq!(server, Some(("acid@alice", "atag", Some("svca"), Some("a"))), "the originator's identity, its To-tag and leg");
        assert!(names_server_branch(&call, RELAYED_BRANCH), "the relayed INVITE's branch is a server seed");
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
        leg.dialogs[0].ext.pending_invite_txn = Some(b_handle(REINVITE_BRANCH, 2));
        leg.dialogs[0].ext.ack_branch = None;
        let open = call_with(LegState::Confirmed, leg.clone());
        let seeds = seeds_for(&open);
        assert!(server_branches(&seeds).is_empty(), "nothing was relayed, so no server seed");
        assert_eq!(client_branches(&seeds), vec![REINVITE_BRANCH.to_string()]);

        let mut answered = leg.clone();
        answered.dialogs[0].ext.awaited_ack_cseq = Some(2);
        assert!(seeds_for(&call_with(LegState::Confirmed, answered)).is_empty(), "a 2xx taken closes the round");

        let rejected = call::helpers::close_rejected_invite_round(open, "b-1", REINVITE_BRANCH);
        assert!(seeds_for(&rejected).is_empty(), "a non-2xx final closed the round on the record");
    }

    /// The initial INVITE's handle is closed by nothing but the leg's state: a
    /// non-2xx final on a still-early leg leaves it for the CANCEL path.
    #[test]
    fn closing_a_round_touches_only_a_confirmed_leg() {
        let call = call_with(LegState::Early, b_leg(LegState::Early, Some(b_handle(B_BRANCH, 1))));
        let call = call::helpers::close_rejected_invite_round(call, "b-1", B_BRANCH);
        assert!(call.b_legs[0].dialogs[0].ext.pending_invite_txn.is_some());
    }

    /// A relayed re-INVITE already CANCELled has had its finals from the node
    /// that held it: neither half is in flight.
    #[test]
    fn a_cancelled_relayed_reinvite_seeds_nothing() {
        let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
        leg.dialogs[0].ext.pending_invite_txn = Some(b_handle(REINVITE_BRANCH, 2));
        leg.dialogs[0].ext.ack_branch = None;
        leg.dialogs[0].ext.inbound_pending_requests.push(relayed_reinvite(2, true));
        assert!(seeds_for(&call_with(LegState::Confirmed, leg)).is_empty());
    }

    /// A b-leg whose handle does not read seeds only what does.
    #[test]
    fn an_unreadable_handle_is_skipped() {
        let mut handle = b_handle(B_BRANCH, 1);
        handle.original_invite = b"garbage".to_vec();
        let call = call_with(LegState::Early, b_leg(LegState::Trying, Some(handle)));
        let seeds = seeds_for(&call);
        assert_eq!(server_branches(&seeds), vec![(A_BRANCH.to_string(), true)]);
        assert!(client_branches(&seeds).is_empty());
    }
}
