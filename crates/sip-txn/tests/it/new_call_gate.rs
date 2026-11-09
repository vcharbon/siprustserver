//! `TransactionConfig::new_call_gate`: while the consumer's switch is set, a
//! new normal INVITE is refused at receipt with the node's shared refusal,
//! before any 100 Trying, transaction or event, and counted once; its copy
//! draws the same refusal as a copy. Emergency and in-dialog INVITEs pass, and
//! with the switch off every INVITE is admitted. A gate with an age does the
//! same while the oldest event in the queue has waited it, whatever the switch
//! reads: its consumer is held in one piece of work and writes nothing.

use crate::common;
use common::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipRequest, SipResponse};
use sip_txn::{
    IdGen, InviteClass, InviteRefusals, NewCallGate, TransactionConfig, TransactionEvent,
};

/// A stateless refusal under a request-derived To-tag.
fn refuse_503(req: &SipRequest) -> Result<SipResponse, sip_message::generators::GenerateError> {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(format!("refused-{}", req.call_id().as_str().replace('@', "."))),
            ..Default::default()
        },
    )
}

/// A layer whose new-call gate reads `switch`, refusing through a shared
/// 503, classifying by `class_of`.
async fn gated(switch: &Arc<AtomicBool>) -> Stack {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    Stack::build_with_config(
        1,
        1024,
        TransactionConfig {
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_refusals: Some(refusals),
            new_call_gate: Some(NewCallGate::new(switch.clone(), sip_txn::class_of)),
            ..common::config()
        },
    )
    .await
}

fn invites(events: &[TransactionEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(e, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Request(r) if r.method() == "INVITE"))
        })
        .count()
}

/// The INVITE `inbound_request` builds, with `extra` header lines.
fn invite_with(branch: &str, call_id: &str, to_tag: Option<&str>, extra: &str) -> Vec<u8> {
    let raw = String::from_utf8(inbound_request("INVITE", branch, call_id, to_tag)).unwrap();
    raw.replacen("Content-Length: 0\r\n", &format!("{extra}Content-Length: 0\r\n"), 1).into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_new_normal_invite_is_refused_at_receipt_and_its_copy_draws_that_refusal() {
    let switch = Arc::new(AtomicBool::new(true));
    let mut stack = gated(&switch).await;
    let invite = invite_with("z9hG4bK-gated", "gated@unit", None, "");

    stack.inject(&invite).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 503), 1, "refused at once, no 100");
    assert_eq!(invites(&stack.drain_events()), 0, "nothing handed up");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "no transaction");
    assert_eq!(stack.txn.metrics().new_call_gate_refused(InviteClass::Normal), 1);

    stack.inject(&invite).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 503), 1, "the copy draws the refusal");
    assert_eq!(stack.txn.metrics().new_call_gate_refused(InviteClass::Normal), 1, "counted once");
    assert_eq!(stack.txn.metrics().refused_copies(), 1, "the copy counted as a copy");
}

/// The age the gate of [`aged`] refuses at.
const AGE_MS: u64 = 450;

/// A layer whose gate's switch stays off and which refuses on the queue's age
/// alone.
async fn aged() -> Stack {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    let switch = Arc::new(AtomicBool::new(false));
    let gate = NewCallGate::new(switch, sip_txn::class_of)
        .refusing_at_age(std::time::Duration::from_millis(AGE_MS));
    Stack::build_with_config(
        1,
        1024,
        TransactionConfig {
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_refusals: Some(refusals),
            new_call_gate: Some(gate),
            ..common::config()
        },
    )
    .await
}

/// A consumer that takes nothing writes no switch: the event it left waiting
/// reaches the gate's age, and a new normal INVITE draws the shared refusal, no
/// 100, no event, counted once; its copy draws the same refusal.
#[tokio::test(start_paused = true)]
async fn a_new_invite_is_refused_once_the_oldest_event_reaches_the_age() {
    let mut stack = aged().await;
    stack.inject(&invite_with("z9hG4bK-first", "first@unit", None, "")).await;
    elapse_ms(AGE_MS - 10).await;
    let peer_before = stack.drain_peer();
    assert_eq!(count_responses(&peer_before, 503), 0, "the first INVITE is admitted");

    let late = invite_with("z9hG4bK-late", "late@unit", None, "");
    elapse_ms(10).await;
    stack.inject(&late).await;
    elapse_ms(50).await;
    let at_the_peer = stack.drain_peer();
    assert_eq!(count_responses(&at_the_peer, 503), 1, "the refusal");
    let trying = at_the_peer.iter().filter(|m| {
        matches!(m, SipMessage::Response(r)
            if r.status() == 100 && r.call_id().as_str() == "late@unit")
    });
    assert_eq!(trying.count(), 0, "no 100 for the refused INVITE");
    assert_eq!(stack.txn.metrics().new_call_gate_refused(InviteClass::Normal), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "the first INVITE's alone");

    stack.inject(&late).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 503), 1, "the copy draws the refusal");
    assert_eq!(stack.txn.metrics().new_call_gate_refused(InviteClass::Normal), 1, "counted once");
    assert_eq!(stack.txn.metrics().refused_copies(), 1);

    assert_eq!(invites(&stack.drain_events()), 1, "the first INVITE alone reached the queue");
}

/// An event that has waited less than the age refuses nothing, and a queue
/// drained reads young again at once.
#[tokio::test(start_paused = true)]
async fn a_wait_under_the_age_refuses_nothing_and_a_drained_queue_reads_young() {
    let mut stack = aged().await;
    stack.inject(&invite_with("z9hG4bK-a", "a@unit", None, "")).await;
    elapse_ms(AGE_MS - 10).await;
    stack.inject(&invite_with("z9hG4bK-b", "b@unit", None, "")).await;
    elapse_ms(5).await;
    assert_eq!(invites(&stack.drain_events()), 2, "both admitted");
    assert_eq!(count_responses(&stack.drain_peer(), 503), 0);

    elapse_ms(2 * AGE_MS).await;
    stack.inject(&invite_with("z9hG4bK-c", "c@unit", None, "")).await;
    elapse_ms(5).await;
    assert_eq!(invites(&stack.drain_events()), 1, "the queue is empty, however long ago");
    let refused = InviteClass::ALL.map(|c| stack.txn.metrics().new_call_gate_refused(c));
    assert_eq!(refused, [0; 3]);
}

/// Past the age, an emergency INVITE and an in-dialog one pass as they do on
/// the switch.
#[tokio::test(start_paused = true)]
async fn past_the_age_emergency_and_in_dialog_invites_pass() {
    let mut stack = aged().await;
    stack.inject(&invite_with("z9hG4bK-first", "first@unit", None, "")).await;
    elapse_ms(AGE_MS + 10).await;
    let emergency = invite_with("z9hG4bK-sos", "sos@unit", None, "Resource-Priority: esnet.0\r\n");
    stack.inject(&emergency).await;
    stack.inject(&invite_with("z9hG4bK-re", "re@unit", Some("b2bua-tag"), "")).await;
    elapse_ms(5).await;
    assert_eq!(invites(&stack.drain_events()), 3, "the first INVITE and both that pass");
    assert_eq!(count_responses(&stack.drain_peer(), 503), 0);
    let refused = InviteClass::ALL.map(|c| stack.txn.metrics().new_call_gate_refused(c));
    assert_eq!(refused, [0; 3]);
}

#[tokio::test(start_paused = true)]
async fn emergency_and_in_dialog_invites_pass_and_an_open_gate_admits_every_invite() {
    let switch = Arc::new(AtomicBool::new(true));
    let mut stack = gated(&switch).await;

    let emergency = invite_with("z9hG4bK-sos", "sos@unit", None, "Resource-Priority: esnet.0\r\n");
    stack.inject(&emergency).await;
    let reinvite = invite_with("z9hG4bK-re", "re@unit", Some("b2bua-tag"), "");
    stack.inject(&reinvite).await;
    elapse_ms(50).await;
    assert_eq!(invites(&stack.drain_events()), 2, "both handed up");

    switch.store(false, Ordering::Relaxed);
    stack.inject(&invite_with("z9hG4bK-open", "open@unit", None, "")).await;
    elapse_ms(50).await;
    assert_eq!(invites(&stack.drain_events()), 1, "an open gate admits a normal INVITE");
    assert_eq!(count_responses(&stack.drain_peer(), 503), 0, "nothing refused");
    let refused = InviteClass::ALL.map(|c| stack.txn.metrics().new_call_gate_refused(c));
    assert_eq!(refused, [0; 3]);
}
