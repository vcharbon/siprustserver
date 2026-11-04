//! `TransactionConfig::new_call_gate`: while the consumer's switch is set, a
//! new normal INVITE is refused at receipt with the node's shared refusal,
//! before any 100 Trying, transaction or event, and counted once; its copy
//! draws the same refusal as a copy. Emergency and in-dialog INVITEs pass, and
//! with the switch off every INVITE is admitted.

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
