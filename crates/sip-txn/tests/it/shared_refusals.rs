//! The layer shares its refusals of new INVITEs with the stages ahead of it
//! ([`InviteRefusals`]): a copy of an INVITE refused there draws that refusal
//! here, before any 100 Trying or transaction (RFC 3261 §8.2.7), and its ACK
//! ends here; an initial INVITE the layer admits is held there while its
//! transaction lives, so no stage ahead refuses a copy of it (§17.2.1).

use crate::common;
use common::*;
use std::sync::Arc;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{serialize, CustomParser, SipMessage, SipParser, SipRequest, SipResponse};
use sip_txn::{
    IdGen, InviteRefusals, TransactionConfig, TransactionEvent, TxnKind, TxnSeed, Verdict,
};

/// A stateless refusal under a request-derived To-tag.
fn refuse_503(req: &SipRequest) -> SipResponse {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(format!("refused-{}", req.call_id().as_str())),
            ..Default::default()
        },
    )
}

/// A layer sharing `refusals`, with no deferred-backlog ceiling of its own.
async fn sharing(refusals: &InviteRefusals) -> Stack {
    Stack::build_with_config(
        1,
        1024,
        TransactionConfig {
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_refusals: Some(refusals.clone()),
            ..Default::default()
        },
    )
    .await
}

fn invite_bytes() -> Vec<u8> {
    inbound_request("INVITE", "z9hG4bK-shared", "shared@unit", None)
}

fn invite() -> SipRequest {
    match CustomParser::new().parse(&invite_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

fn delivered_invites(events: &[TransactionEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(e, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Request(r) if r.method() == "INVITE"))
        })
        .count()
}

/// A stage ahead refused the INVITE: the copy that reaches the layer draws the
/// same bytes, with no 100 Trying, no transaction and no event, and the
/// caller's ACK ends in the layer.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_invite_refused_ahead_draws_that_refusal() {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    let mut stack = sharing(&refusals).await;
    assert_eq!(refusals.refuse(&invite()), Verdict::Refused { first: true });

    stack.inject(&invite_bytes()).await;
    elapse_ms(50).await;
    let wire: Vec<Vec<u8>> =
        stack.drain_peer().into_iter().map(|m| serialize(&m)).collect::<Vec<_>>();
    let expected = serialize(&SipMessage::Response(refusals.answer(&invite())));
    assert_eq!(wire, [expected], "the same 503, and nothing else");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    assert_eq!(delivered_invites(&stack.drain_events()), 0);

    let ack = inbound_request("ACK", "z9hG4bK-shared", "shared@unit", Some("refused-shared@unit"));
    stack.inject(&ack).await;
    elapse_ms(50).await;
    assert!(stack.drain_events().is_empty(), "the ACK to the refusal ends in the layer");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

/// An initial INVITE the layer admits is held while its transaction lives: a
/// stage ahead spares its copies. Once the transaction leaves, the identity
/// is judged afresh.
#[tokio::test(start_paused = true)]
async fn an_admitted_invite_is_held_while_its_transaction_lives() {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    let mut stack = sharing(&refusals).await;

    stack.inject(&invite_bytes()).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1);
    assert_eq!(delivered_invites(&stack.drain_events()), 1);
    assert_eq!(refusals.refuse(&invite()), Verdict::Held);
    assert!(!refusals.remembers(), "sparing an INVITE refuses nothing");

    let busy = generate_response(
        &invite(),
        486,
        "Busy Here",
        &GenerateResponseOpts { to_tag: Some("uas-busy".into()), ..Default::default() },
    );
    stack.txn.send_response(busy, addr(PEER)).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1);
    stack.inject(&inbound_request("ACK", "z9hG4bK-shared", "shared@unit", Some("uas-busy"))).await;
    elapse_ms(40_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "Timer I ended the transaction");
    assert_eq!(refusals.refuse(&invite()), Verdict::Refused { first: true });
}

/// A server INVITE rebuilt from a record at a takeover is held too, so a stage
/// ahead spares the caller's copies of it.
#[tokio::test(start_paused = true)]
async fn a_seeded_server_invite_is_held() {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    let stack = sharing(&refusals).await;
    let seeded = stack
        .txn
        .seed(
            "call-1",
            vec![TxnSeed::ServerInvite {
                branch: "z9hG4bK-shared".into(),
                sent_by: peer_sent_by(),
                call_id: "shared@unit".into(),
                from_tag: "caller-tag".into(),
                to_tag: Some("uas-early".into()),
                leg_id: Some("a".into()),
                original_request: Some(invite()),
            }],
        )
        .await
        .unwrap();
    assert_eq!(seeded, 1);
    assert_eq!(refusals.refuse(&invite()), Verdict::Held);
}

/// A seed wins over a refusal: the call exists on this node. An INVITE a
/// stage ahead refused and this layer then seeds is held, not refused, so its
/// next copy is the seed's to absorb — a 100 Trying, no 503.
#[tokio::test(start_paused = true)]
async fn a_seed_clears_a_refusal_of_its_invite() {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    let stack = sharing(&refusals).await;
    assert_eq!(refusals.refuse(&invite()), Verdict::Refused { first: true });
    stack
        .txn
        .seed(
            "call-1",
            vec![TxnSeed::ServerInvite {
                branch: "z9hG4bK-shared".into(),
                sent_by: peer_sent_by(),
                call_id: "shared@unit".into(),
                from_tag: "caller-tag".into(),
                to_tag: Some("uas-early".into()),
                leg_id: Some("a".into()),
                original_request: Some(invite()),
            }],
        )
        .await
        .unwrap();
    assert!(!refusals.refused(&invite()), "the seeded INVITE is no longer refused");
    assert_eq!(refusals.refuse(&invite()), Verdict::Held);

    stack.inject(&invite_bytes()).await;
    elapse_ms(50).await;
    let wire = stack.drain_peer();
    assert_eq!((count_responses(&wire, 100), count_responses(&wire, 503)), (1, 0));
}

/// A client transaction this node sends on a held server INVITE's branch is
/// another transaction (RFC 3261 §17.1.3, §17.2.3): the server INVITE stays,
/// its hold with it, and a copy of the INVITE still draws its 100.
#[tokio::test(start_paused = true)]
async fn a_client_transaction_on_the_branch_leaves_the_hold() {
    let refusals = InviteRefusals::new(Arc::new(refuse_503));
    let mut stack = sharing(&refusals).await;
    stack.inject(&invite_bytes()).await;
    elapse_ms(50).await;
    stack.drain_events();
    stack.drain_peer();
    assert_eq!(refusals.refuse(&invite()), Verdict::Held);

    let options = outbound_request("OPTIONS", "z9hG4bK-shared");
    stack.txn.send_request(options, addr(PEER), TxnKind::NonInvite).await.unwrap();
    elapse_ms(10).await;
    assert_eq!(refusals.refuse(&invite()), Verdict::Held, "the server INVITE keeps its hold");

    stack.inject(&invite_bytes()).await;
    elapse_ms(10).await;
    let wire = stack.drain_peer();
    assert_eq!((count_responses(&wire, 100), count_responses(&wire, 503)), (1, 0));
    assert!(stack.drain_events().is_empty(), "the copy is absorbed");
}
