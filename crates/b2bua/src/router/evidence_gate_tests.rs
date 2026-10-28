//! What the dialog gate lets through to the evidence rules (ADR-0055 §4): an
//! ACK of the caller's unanswered INVITE reaches them only on a call a
//! takeover copy served, and a 2xx on a leg the record lacks only under a
//! via-extension the cluster key verifies (ADR-0055 §12).

use b2bua_sdk::event::CallEvent;
use call::{Call, Direction};
use sip_message::header::HeaderValue;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipRequest};

use super::dialog_gate::{admit, Gate};
use super::resolve::Resolution;
use super::test_support::{invite, node_with, src};
use super::RouterCtx;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};
use crate::store::CallTopology;

fn key() -> sip_ids::SigningKey {
    sip_ids::SigningKey::new(&[7u8; 32]).expect("a 32-byte key")
}

/// A call `w0` serves with `w1` as its backup, its caller's INVITE
/// unanswered, resident on `ctx` at `(3, bak_gen)`; `bak_gen`: the writes of
/// a copy the record took.
fn unanswered_call(ctx: &RouterCtx, bak_gen: i64) -> Call {
    let call = build_initial_call(
        &invite("w0", "w1", "evidence-gate"),
        src(),
        &B2buaConfig { self_ordinal: "w0".into(), ..Default::default() },
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    ctx.state.remove(&call.call_ref);
    ctx.state.create(call.clone(), Some(CallTopology::new("w0", "w1", 3, bak_gen)));
    call
}

/// The caller's ACK of her INVITE, naming the dialog `to_tag`.
fn caller_ack(to_tag: &str) -> SipRequest {
    let inv = invite("w0", "w1", "evidence-gate");
    inv.thaw()
        .with_method(Method::Ack)
        .set(inv.to().clone().with_tag(to_tag))
        .set(sip_message::header::CSeq::new(inv.cseq().seq(), Method::Ack))
        .freeze()
        .expect("the ACK is complete")
}

fn resolution(call: &Call, leg: &str) -> Resolution {
    Resolution {
        call_ref: Some(call.call_ref.clone()),
        source_leg_id: leg.into(),
        direction: if leg == "a" { Direction::FromA } else { Direction::FromB },
        initial_invite: false,
        incarnation: None,
        indexed: None,
    }
}

/// What the gate made of an event, for a failure message.
fn kind(gate: &Gate) -> &'static str {
    match gate {
        Gate::Pass(_) => "passed",
        Gate::Answered { .. } => "answered here",
        Gate::Dropped => "dropped",
    }
}

fn event(message: SipMessage) -> CallEvent {
    CallEvent::sip(message, src())
}

#[tokio::test(start_paused = true)]
async fn an_ack_of_the_unanswered_caller_invite_is_evidence_once_a_copy_served_the_call() {
    let node = node_with("w0", |c| c.cluster_key = Some(key())).await;
    let ctx = node.core.router_ctx();
    let ack = event(SipMessage::Request(caller_ack("answered-elsewhere")));

    let unserved = unanswered_call(ctx, 0);
    let gate = admit(ctx, &unserved, &ack, &resolution(&unserved, "a"));
    assert!(
        matches!(gate, Gate::Answered { answer: None, .. }),
        "no copy served the call: the copy that answered her is live and writing, {}",
        kind(&gate)
    );

    let served = unanswered_call(ctx, 1);
    let gate = admit(ctx, &served, &ack, &resolution(&served, "a"));
    assert!(matches!(gate, Gate::Pass(_)), "a copy's write taken here: evidence, {}", kind(&gate));
}

/// A 2xx to the INVITE of `leg`, its Via ours with the via-extension signed
/// under `signing` when given.
fn answer_on(call: &Call, leg: &str, signing: Option<&sip_ids::SigningKey>) -> SipMessage {
    let facts = RequestFacts { method: "INVITE", uri: "sip:carol@10.0.0.3:5070", offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: "127.0.0.3",
            local_port: 5080,
            call_ref: &call.call_ref,
            leg,
            incarnation_mark: call.incarnation_mark(),
            is_emergency: false,
            other_owner: Some("w0"),
            request: signing.map(|k| (facts, k)),
        },
        "z9hG4bKcarol".to_string(),
    );
    let raw = format!(
        "SIP/2.0 200 OK\r\nVia: {}\r\nFrom: <sip:svc@127.0.0.3>;tag=svc\r\n\
         To: <sip:carol@10.0.0.3>;tag=caroltag\r\nCall-ID: carol@x\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:carol@10.0.0.3:5070>\r\nContent-Length: 0\r\n\r\n",
        via.to_wire()
    );
    CustomParser::new().parse(raw.as_bytes()).expect("a response")
}

#[tokio::test(start_paused = true)]
async fn a_2xx_on_a_leg_the_record_lacks_reaches_the_rules_under_a_verified_extension_only() {
    let node = node_with("w0", |c| c.cluster_key = Some(key())).await;
    let ctx = node.core.router_ctx();
    let call = unanswered_call(ctx, 0);

    let signed = event(answer_on(&call, "b-7", Some(&key())));
    let gate = admit(ctx, &call, &signed, &resolution(&call, "b-7"));
    assert!(matches!(gate, Gate::Pass(_)), "trigger 5 reaches its rule, {}", kind(&gate));

    let other_key = sip_ids::SigningKey::new(&[8u8; 32]).expect("a key");
    for (what, unverified) in [
        ("unsigned", answer_on(&call, "b-7", None)),
        ("signed under another key", answer_on(&call, "b-7", Some(&other_key))),
    ] {
        let gate = admit(ctx, &call, &event(unverified), &resolution(&call, "b-7"));
        assert!(matches!(gate, Gate::Dropped), "{what}: a stray, {}", kind(&gate));
    }
}

#[tokio::test(start_paused = true)]
async fn her_ack_reaches_the_turn_only_while_its_dialog_is_held_possibly_confirmed() {
    let node = node_with("w0", |c| c.cluster_key = Some(key())).await;
    let ctx = node.core.router_ctx();
    let mut call = unanswered_call(ctx, 1);
    let tag = "held-tag";
    let mut d =
        call::Dialog::new(tag, call.a_leg_invite.from_tag.clone(), call::DialogState::Early, 1);
    d.state = call::DialogState::Ended(call::DialogEnd::Rejected(503));
    call.a_leg.dialogs.insert(d);
    *call.a_leg.invite_final_sent_mut() = Some(503);
    let ack = event(SipMessage::Request(caller_ack(tag)));

    let gate = admit(ctx, &call, &ack, &resolution(&call, "a"));
    assert!(matches!(gate, Gate::Answered { answer: None, .. }), "not held: {}", kind(&gate));

    let key = call::TimerKey::call(call::TimerType::PossiblyConfirmed { tag: tag.into() });
    call::helpers::arm_timer(&mut call, key, 32_000);
    let gate = admit(ctx, &call, &ack, &resolution(&call, "a"));
    assert!(matches!(gate, Gate::Pass(_)), "held: her ACK ends the hold, {}", kind(&gate));
}
