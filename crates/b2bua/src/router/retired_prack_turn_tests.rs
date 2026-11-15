//! A caller's PRACK on a caller-facing dialog a sibling fork's answer retired,
//! through the real turn (RFC 3262 §3): it is answered here and never relayed,
//! even where the record still holds the callee's fork early and owed a PRACK
//! — the state a takeover copy reads when the owner's 2xx turn never reached it.

use b2bua_sdk::event::CallEvent;
use call::helpers::Resolved;
use call::{
    CSeq, Call, Dialog, DialogId, DialogState, Direction, Ladder, OutboundInvite, Provisional,
    ProvisionalId, RSeq, ResponderAck, Shown,
};
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;

use super::test_support::{invite, node, src, turn};
use crate::config::B2buaConfig;
use crate::effects::{HandlerResult, OutboundBody};
use crate::initial_invite::build_initial_call;
use crate::rule_chain::{rule_chain_turn, TurnInput};

/// The caller-facing tag of the fork the answer retired.
const RETIRED: &str = "a-fork-b";

/// A call answered on the callee's fork A, the caller's fork-B dialog retired
/// with a reliable 183 shown there unacknowledged, and the callee's fork B
/// still early with that 183 owed a PRACK.
fn answered_on_a_with_fork_b_owed() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "retired-prack"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut a_fork_a = Dialog::new("a-fork-a", "alicetag", DialogState::Confirmed, 1);
    a_fork_a.remote_target = "sip:alice@10.0.0.9:5060".into();
    let mut a_fork_b = Dialog::new(RETIRED, "alicetag", DialogState::Retired, 1);
    a_fork_b.shown = true;
    a_fork_b.peer_cseq = Some(1);
    call.a_leg.dialogs = vec![a_fork_a, a_fork_b].into();
    call.a_leg.own_tag = Some("a-fork-a".into());
    call::testkit::stage_leg(&mut call.a_leg, call::LegState::Confirmed);
    *call.a_leg.invite_final_sent_mut() = Some(200);

    let mut b = call::testkit::dialled_leg("b-1", "svc");
    b.call_id = "b-cid@10.0.0.2".into();
    b.invite = Some(call::RequestRecord::dialled(1, None, OutboundInvite::new("svc", 0)));
    let mut bob_a = Dialog::new("svc", "forkA", DialogState::Confirmed, 1);
    bob_a.remote_target = "sip:bob@10.0.0.2:5060".into();
    let mut bob_b = Dialog::new("svc", "forkB", DialogState::Early, 1);
    bob_b.remote_target = "sip:bob@10.0.0.2:5060".into();
    b.dialogs = vec![bob_a, bob_b].into();
    call::helpers::add_b_leg(&mut call, b);

    call.provisionals.push(Provisional {
        id: ProvisionalId {
            leg: "b-1".into(),
            responder_tag: "forkB".into(),
            invite_cseq: CSeq(1),
            rseq: RSeq(1),
        },
        responder_sdp: false,
        responder: ResponderAck::Pending,
        shown: Some(Shown {
            tag: RETIRED.into(),
            rseq: RSeq(7),
            invite_cseq: CSeq(1),
            carried_sdp: false,
            ladder: Ladder::Unsent,
        }),
    });
    call
}

/// The caller's PRACK on the retired fork naming `rseq`.
fn caller_prack(rseq: u32) -> CallEvent {
    let raw = format!(
        "PRACK sip:b2bua@127.0.0.2:5080;leg=a SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKprack{rseq}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=alicetag\r\n\
To: <sip:bob@example.com>;tag={RETIRED}\r\n\
Call-ID: retired-prack@10.0.0.9\r\n\
CSeq: 2 PRACK\r\n\
RAck: {rseq} 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let message = CustomParser::new().parse(raw.as_bytes()).expect("a PRACK");
    CallEvent::sip(message, "10.0.0.9:5060".parse().unwrap())
}

/// One turn of `event` from the caller, resolved to the retired fork.
async fn caller_turn(event: &CallEvent) -> HandlerResult {
    let node = node("w0").await;
    let call = answered_on_a_with_fork_b_owed();
    let call_ref = call.call_ref.clone();
    let on_fork_b = DialogId { our_tag: RETIRED.into(), peer_tag: "alicetag".into() };
    let input = TurnInput {
        event,
        call_ref: &call_ref,
        source_leg_id: "a",
        direction: Direction::FromA,
        resolved: Resolved { dialog: Some(on_fork_b), record: None },
        matched_client_txn: false,
        now_ms: 1_000,
        owners: Default::default(),
    };
    rule_chain_turn(&node.core.router_ctx().framework(), call, input, &mut turn(1))
        .expect("the turn crosses no cap")
}

/// Each message `result` sends: `(leg, status or method)`.
fn sent(result: &HandlerResult) -> Vec<(String, String)> {
    result
        .effects
        .outbound
        .iter()
        .filter_map(|e| {
            let what = match &e.body {
                OutboundBody::Response(r) => r.status().to_string(),
                OutboundBody::Request(r) => r.method().as_str().to_string(),
                OutboundBody::Datagram(_) => return None,
            };
            Some((e.leg_id.clone().unwrap_or_default(), what))
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_prack_naming_the_shown_183_is_answered_here_and_never_relayed() {
    let result = caller_turn(&caller_prack(7)).await;
    assert_eq!(sent(&result), [("a".to_string(), "200".to_string())]);
}

#[tokio::test(start_paused = true)]
async fn a_prack_naming_nothing_shown_there_is_refused() {
    let result = caller_turn(&caller_prack(8)).await;
    assert_eq!(sent(&result), [("a".to_string(), "481".to_string())]);
}
