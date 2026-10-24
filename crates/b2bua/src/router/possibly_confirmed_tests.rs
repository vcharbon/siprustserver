//! The possibly-confirmed hold (ADR-0055 §4) through whole turns: on a call a
//! takeover copy serves, the callee's BYE on a dialog the record holds early
//! proves it answered, so the caller, whom the record holds unanswered, may
//! hold the 2xx the dead owner relayed. She is sent her non-2xx under the tag
//! that 2xx carried and that dialog is BYEd on her ACK or at 64·T1; on a call
//! one node served the caller is answered and nothing is held.

use b2bua_sdk::event::CallEvent;
use call::helpers::Resolved;
use call::{Call, Dialog, DialogId, DialogState, Direction, OutboundInvite, TimerKey, TimerType};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::test_support::{invite, node, src, turn};
use crate::config::B2buaConfig;
use crate::effects::{HandlerResult, OutboundBody};
use crate::initial_invite::build_initial_call;
use crate::rule_chain::{rule_chain_turn, TurnInput};

/// A call `w0` serves with `w1` as its backup, ringing at `b-1` on the
/// callee's early dialog `b2bua-b-tag`/`bob-tag`, the caller unanswered.
fn ringing() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let call = build_initial_call(
        &invite("w0", "w1", "possibly-confirmed"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = "b-cid@10.0.0.2".into();
    b.invite = Some(call::RequestRecord::dialled(1, None, OutboundInvite::new("b2bua-b-tag", 0)));
    b.own_tag = None;
    *b.invite_final_sent_mut() = None;
    let mut bob = Dialog::new("b2bua-b-tag", "bob-tag", DialogState::Early, 1);
    bob.remote_target = "sip:bob@10.0.0.2:5060".into();
    b.dialogs = vec![bob].into();
    call::helpers::add_b_leg(call, b)
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).expect("a message")
}

/// The callee's BYE on his early dialog.
fn bye_from_bob() -> CallEvent {
    let raw = "BYE sip:b2bua@127.0.0.2:5080;leg=b-1 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bKbobbye\r\n\
Max-Forwards: 70\r\n\
From: <sip:bob@10.0.0.2>;tag=bob-tag\r\n\
To: <sip:svc@127.0.0.2>;tag=b2bua-b-tag\r\n\
Call-ID: b-cid@10.0.0.2\r\n\
CSeq: 1 BYE\r\n\
Content-Length: 0\r\n\r\n";
    CallEvent::sip(parse(raw), "10.0.0.2:5060".parse().unwrap())
}

/// One turn of `event` on `leg` under `node`'s framework.
fn turn_on(
    node: &super::test_support::Node,
    call: Call,
    event: &CallEvent,
    leg: &str,
    resolved: Resolved,
    now_ms: i64,
) -> HandlerResult {
    let call_ref = call.call_ref.clone();
    let placed = crate::router::topology_cookie::placement(&call);
    let owners =
        node.core.router_ctx().owners_of(&crate::store::Enveloped::new(call.clone(), placed));
    let input = TurnInput {
        event,
        call_ref: &call_ref,
        source_leg_id: leg,
        direction: if leg == "a" { Direction::FromA } else { Direction::FromB },
        resolved,
        matched_client_txn: false,
        now_ms,
        owners,
    };
    let fw = node.core.router_ctx().framework();
    rule_chain_turn(&fw, call, input, &mut turn(1))
}

/// The callee's BYE on a call `ordinal` serves.
fn bob_hangs_up_at(node: &super::test_support::Node) -> HandlerResult {
    let on_bob = DialogId { our_tag: "b2bua-b-tag".into(), peer_tag: "bob-tag".into() };
    let resolved = Resolved { dialog: Some(on_bob), record: None };
    turn_on(node, ringing(), &bye_from_bob(), "b-1", resolved, 1_000)
}

/// What `result` sends the caller: `(status or method, To- or From-tag ours)`.
fn toward_alice(result: &HandlerResult) -> Vec<(String, String)> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .filter_map(|e| match &e.body {
            OutboundBody::Response(r) => {
                Some((r.status().to_string(), r.to().tag().unwrap_or_default().to_string()))
            }
            OutboundBody::Request(r) => Some((
                r.method().as_str().to_string(),
                r.from().tag().unwrap_or_default().to_string(),
            )),
            OutboundBody::Datagram(_) => None,
        })
        .collect()
}

/// The caller dialog `result`'s call holds possibly confirmed.
fn held(result: &HandlerResult) -> Option<(String, i64)> {
    result.call.timers.iter().find_map(|t| match t.timer_type() {
        TimerType::PossiblyConfirmed { tag } => Some((tag.clone(), t.fire_at)),
        _ => None,
    })
}

#[tokio::test(start_paused = true)]
async fn a_copy_holds_the_caller_dialog_the_callee_answered_and_byes_it_at_its_deadline() {
    let copy = node("w1").await;
    let first = bob_hangs_up_at(&copy);
    let (tag, until) = held(&first).expect("the caller dialog is held");
    assert_eq!(
        toward_alice(&first),
        [("503".to_string(), tag.clone())],
        "her non-2xx under the tag the dead owner's 2xx carried, no BYE before her ACK or 64·T1"
    );
    assert_eq!(until, 1_000 + 32_000, "held for 64·T1");
    assert_eq!(first.call.state, call::CallModelState::Terminating, "the call waits for it");

    let fire = CallEvent::Timer {
        key: TimerKey::call(TimerType::PossiblyConfirmed { tag: tag.clone() }),
        generation: 0,
        call_ref: first.call.call_ref.clone(),
        incarnation: None,
    };
    let last = turn_on(&copy, first.call, &fire, "a", Resolved::default(), until);
    assert_eq!(toward_alice(&last), [("BYE".to_string(), tag)], "the held dialog is BYEd");
    assert_eq!(held(&last), None);
}

#[tokio::test(start_paused = true)]
async fn her_ack_naming_the_held_dialog_draws_its_bye_at_once() {
    let copy = node("w1").await;
    let first = bob_hangs_up_at(&copy);
    let (tag, _) = held(&first).expect("the caller dialog is held");
    let inv = crate::test_call::caller_of(&first.call);
    let ack = inv
        .thaw()
        .with_method(sip_message::Method::Ack)
        .set(inv.to().clone().with_tag(tag.as_str()))
        .set(sip_message::header::CSeq::new(inv.cseq().seq(), sip_message::Method::Ack))
        .freeze()
        .expect("the ACK is complete");
    let event = CallEvent::sip(SipMessage::Request(ack), src());
    let acked = turn_on(&copy, first.call, &event, "a", Resolved::default(), 2_000);
    assert_eq!(toward_alice(&acked), [("BYE".to_string(), tag)], "her ACK proves the 2xx");
    assert_eq!(held(&acked), None);
}

#[tokio::test(start_paused = true)]
async fn on_a_call_one_node_served_the_caller_is_answered_and_nothing_held() {
    let primary = node("w0").await;
    let result = bob_hangs_up_at(&primary);
    assert_eq!(held(&result), None, "no other owner could have answered her");
    let sent = toward_alice(&result);
    assert_eq!(sent.len(), 1, "her one final: {sent:?}");
    assert!(sent[0].0.starts_with('5') || sent[0].0.starts_with('4'), "a non-2xx: {sent:?}");
}
