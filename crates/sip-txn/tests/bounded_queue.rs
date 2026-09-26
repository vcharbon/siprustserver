//! Port of `tests/sip/transaction-layer-bounded-queue.test.ts` — the inbound→
//! app event queue is bounded; producers never block; excess is dropped-newest
//! and counted by reason; draining recovers normal accept.
//!
//! Adaptation: the source pumps incrementally so its tiny UDP recv queue
//! (`udpQueueMax`) never fills. Here the b2bua endpoint is bound with a
//! generous recv queue so the network layer never tail-drops, isolating the
//! event-queue bound under test. (The UDP-queue tail-drop is sip-net's own
//! concern, covered by its `simulated` tests.)

mod common;
use common::*;
use std::sync::Arc;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::{SipMessage, SipRequest};
use sip_txn::{DeferredBound, EventQueueClass, IdGen, TransactionConfig, TransactionEvent};

const UDP_QUEUE_MAX: usize = 8; // event capacity = max(64, 4×8) = 64

#[tokio::test(start_paused = true)]
async fn capacity_and_counters_start_at_zero() {
    let stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_capacity(), std::cmp::max(64, UDP_QUEUE_MAX * 4));
    assert_eq!(m.event_queue_depth(), 0);
    assert_eq!(m.event_queue_deferred(), 0);
    for r in EventQueueClass::ALL {
        assert_eq!(m.event_queue_drops(r), 0);
        assert_eq!(m.event_queue_deferrals(r), 0);
    }
    assert_eq!(m.deferred_swept(), 0);
    assert_eq!((m.deferred_refused(false), m.deferred_refused(true)), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn overflow_stops_at_capacity_and_counts_drops_then_recovers() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    let overflow = cap * 2;

    // No consumer drains `events`, so every parsed (unknown-branch) 180 lands
    // directly in the bounded queue; past `cap` they must drop, not block.
    for i in 0..overflow {
        stack
            .inject(&response_bytes(
                180,
                "Ringing",
                "INVITE",
                &format!("z9hG4bK-overflow-{i}"),
                &format!("overflow-{i}@unit"),
                true,
            ))
            .await;
    }

    // Auto-advance delivers all packets at the transit deadline; the owner
    // then stays continuously runnable (its recv queue is never empty) and so
    // drains the whole burst before the runtime goes idle and wakes us.
    elapse_ms(1_000).await;

    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_depth(), cap, "queue saturated at capacity");
    assert_eq!(m.event_queue_drops(EventQueueClass::Response), (overflow - cap) as u64);
    // Unrelated reasons untouched.
    assert_eq!(m.event_queue_drops(EventQueueClass::RequestInvite), 0);
    assert_eq!(m.event_queue_drops(EventQueueClass::Cancelled), 0);
    assert_eq!(m.event_queue_drops(EventQueueClass::Timeout), 0);

    // Drain `cap` events via the public receiver — queue empties to zero.
    for _ in 0..cap {
        stack.events.recv().await.expect("event");
    }
    assert_eq!(stack.txn.metrics().event_queue_depth(), 0);
}

/// The inbound requests of `method` among `events`.
fn requests_of(events: &[TransactionEvent], method: &str) -> Vec<SipRequest> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Fill the event queue to capacity with responses no transaction matches
/// (they build no transaction and are dropped-newest like any ordinary event).
async fn saturate(stack: &Stack, cap: usize) {
    for i in 0..cap {
        stack
            .inject(&response_bytes(
                180,
                "Ringing",
                "INVITE",
                &format!("z9hG4bK-fill-{i}"),
                &format!("fill-{i}@unit"),
                true,
            ))
            .await;
    }
    elapse_ms(100).await;
    assert_eq!(stack.txn.metrics().event_queue_depth(), cap, "queue saturated");
}

/// RFC 3261 §17.2.2: a non-INVITE request the queue could not take is not
/// admitted. Its retransmission (Timer E, §17.1.2.2) is the redelivery: it
/// reaches the consumer once the queue has room, draws the consumer's answer,
/// and a later copy replays that answer from the transaction.
#[tokio::test(start_paused = true)]
async fn a_non_invite_request_the_full_queue_dropped_is_admitted_on_its_retransmission() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let branch = "z9hG4bK-bye-dropped";
    let bye = inbound_request("BYE", branch, "bye-dropped@unit", Some("dlg-tag"));
    stack.inject(&bye).await;
    elapse_ms(100).await;
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_drops(EventQueueClass::RequestOther), 1, "the drop is counted");
    assert_eq!(m.active_transactions(), 0, "a dropped request holds no server transaction");
    assert!(stack.drain_peer().is_empty(), "nothing answers a request the consumer never saw");

    // The consumer catches up; the peer's Timer E copy arrives.
    assert_eq!(stack.drain_events().len(), cap);
    stack.inject(&bye).await;
    elapse_ms(100).await;
    let delivered = requests_of(&stack.drain_events(), "BYE");
    assert_eq!(delivered.len(), 1, "the retransmission reaches the consumer");

    // The consumer answers; a further copy replays the cached 200 (§17.2.2).
    let ok = generate_response(&delivered[0], 200, "OK", &GenerateResponseOpts::default());
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the BYE is answered");
    stack.inject(&bye).await;
    elapse_ms(100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "cached 200 replayed");
    assert!(stack.drain_events().is_empty(), "an answered copy does not re-surface");

    // Timer J ends the transaction.
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

/// Every copy the full queue refuses is dropped and counted on its own; the
/// first copy that finds room is admitted, once.
#[tokio::test(start_paused = true)]
async fn each_dropped_copy_is_counted_and_the_first_copy_with_room_is_admitted() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let bye = inbound_request("BYE", "z9hG4bK-bye-twice", "bye-twice@unit", Some("dlg-tag"));
    for _ in 0..2 {
        stack.inject(&bye).await;
        elapse_ms(100).await;
    }
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_drops(EventQueueClass::RequestOther), 2, "one per copy");
    assert_eq!(m.active_transactions(), 0);

    assert_eq!(stack.drain_events().len(), cap);
    stack.inject(&bye).await;
    elapse_ms(100).await;
    let delivered = requests_of(&stack.drain_events(), "BYE");
    assert_eq!(delivered.len(), 1, "the third copy reaches the consumer");
    assert_eq!(stack.txn.metrics().event_queue_drops(EventQueueClass::RequestOther), 2);

    let ok = generate_response(&delivered[0], 200, "OK", &GenerateResponseOpts::default());
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

/// A flood of distinct non-INVITE requests into a full queue leaves no
/// transaction behind: what the queue sheds costs the layer nothing.
#[tokio::test(start_paused = true)]
async fn non_invite_requests_shed_by_a_full_queue_leave_no_transaction() {
    let stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let flood = 100;
    for i in 0..flood {
        stack
            .inject(&inbound_request(
                "OPTIONS",
                &format!("z9hG4bK-flood-{i}"),
                &format!("flood-{i}@unit"),
                None,
            ))
            .await;
    }
    elapse_ms(100).await;
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_drops(EventQueueClass::RequestOther), flood as u64);
    assert_eq!(m.active_transactions(), 0);
    assert!(stack.drain_peer().is_empty());
}

/// Pin (holds without the non-INVITE fix): an INVITE whose 100 Trying already
/// silenced the caller's retransmission is its own only delivery (RFC 3261
/// §17.2.1), so a full queue defers it, and it reaches the consumer once the
/// queue has room.
#[tokio::test(start_paused = true)]
async fn an_invite_the_full_queue_could_not_take_is_deferred_not_dropped() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    stack.inject(&inbound_request("INVITE", "z9hG4bK-inv-deferred", "inv@unit", None)).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the 100 Trying left at once");
    let m = stack.txn.metrics();
    assert_eq!(m.active_transactions(), 1, "the INVITE holds its transaction");
    assert_eq!(m.event_queue_deferred(), 1, "the INVITE waits on the deferred backlog");
    assert_eq!(m.event_queue_deferrals(EventQueueClass::RequestInvite), 1);
    assert_eq!(m.event_queue_drops(EventQueueClass::RequestInvite), 0, "a deferral is no drop");

    assert_eq!(stack.drain_events().len(), cap, "the INVITE is not in the saturated queue");
    elapse_ms(150).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(delivered.len(), 1, "the deferred INVITE is delivered once there is room");
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 0);

    // The consumer rejects it; the caller's ACK ends the transaction (§17.2.1).
    let busy = generate_response(
        &delivered[0],
        486,
        "Busy Here",
        &GenerateResponseOpts { to_tag: Some("uas-tag".into()), ..Default::default() },
    );
    stack.txn.send_response(busy, addr(PEER)).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1);
    stack
        .inject(&inbound_request("ACK", "z9hG4bK-inv-deferred", "inv@unit", Some("uas-tag")))
        .await;
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

// ── The deferred backlog is bounded ─────────────────────────────────────────

/// A stateless refusal as a consumer phrases one: 503 with a `Retry-After`
/// and a To-tag derived from the request (RFC 3261 §8.2.7), so every copy of
/// one INVITE draws the same answer.
fn refuse_503(req: &SipRequest) -> sip_message::SipResponse {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(format!("refused-{}", req.call_id().as_str())),
            extra_headers: vec![SipHeader {
                name: "Retry-After".to_string().into(),
                value: "5".to_string().into(),
            }],
            ..Default::default()
        },
    )
}

/// A layer whose deferred backlog refuses new calls at `normal` (and new
/// emergency calls at `emergency`).
async fn bounded_stack(normal: usize, emergency: usize) -> Stack {
    Stack::build_with_config(
        1,
        1024,
        TransactionConfig {
            udp_queue_max: UDP_QUEUE_MAX,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            deferred_bound: Some(DeferredBound {
                normal,
                emergency,
                refusal: Arc::new(refuse_503),
            }),
            ..Default::default()
        },
    )
    .await
}

fn new_invite(i: usize) -> Vec<u8> {
    inbound_request("INVITE", &format!("z9hG4bK-new-{i}"), &format!("new-{i}@unit"), None)
}

/// An initial INVITE carrying the emergency `Resource-Priority` (RFC 4412).
fn emergency_invite(i: usize) -> Vec<u8> {
    let raw = String::from_utf8(inbound_request(
        "INVITE",
        &format!("z9hG4bK-sos-{i}"),
        &format!("sos-{i}@unit"),
        None,
    ))
    .expect("utf-8");
    raw.replace("Content-Length: 0", "Resource-Priority: esnet.0\r\nContent-Length: 0").into_bytes()
}

/// The To-tag of `resp`.
fn to_tag_of(resp: &sip_message::SipResponse) -> String {
    resp.to().tag().expect("a tagged final").to_string()
}

/// The responses of `status` among `msgs`.
fn responses_of(msgs: &[SipMessage], status: u16) -> Vec<sip_message::SipResponse> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == status => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// The ACK a caller owes a non-2xx final (RFC 3261 §17.1.1.3): the INVITE's
/// branch, Call-ID and CSeq number, and the final's To-tag.
fn ack_for(resp: &sip_message::SipResponse) -> Vec<u8> {
    let branch = resp.top_via().branch().expect("branch").to_string();
    inbound_request("ACK", &branch, resp.call_id().as_str(), Some(&to_tag_of(resp)))
}

/// Answer each delivered INVITE 486 and ACK it, as caller and consumer of a
/// rejected call do, then let Timer H/I end every transaction.
async fn reject_and_settle(stack: &mut Stack, delivered: &[SipRequest]) {
    for (i, invite) in delivered.iter().enumerate() {
        let busy = generate_response(
            invite,
            486,
            "Busy Here",
            &GenerateResponseOpts { to_tag: Some(format!("uas-{i}")), ..Default::default() },
        );
        stack.txn.send_response(busy, addr(PEER)).await.unwrap();
    }
    elapse_ms(50).await;
    for busy in responses_of(&stack.drain_peer(), 486) {
        stack.inject(&ack_for(&busy)).await;
    }
    elapse_ms(40_000).await;
    stack.drain_events();
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A consumer that stops draining while initial INVITEs keep arriving: each
/// INVITE past the ceiling is refused before its 100 Trying and holds no
/// transaction, so the backlog stops at the ceiling instead of growing with
/// the flood. The refused copies are answered statelessly — a retransmission
/// draws the same 503 — and once the consumer drains, the deferred INVITEs
/// reach it and complete as ordinary calls.
#[tokio::test(start_paused = true)]
async fn a_stalled_consumer_under_an_invite_flood_holds_the_backlog_at_its_ceiling() {
    let (normal, flood) = (8, 30);
    let mut stack = bounded_stack(normal, normal + 4).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    for i in 0..flood {
        stack.inject(&new_invite(i)).await;
    }
    elapse_ms(100).await;

    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_deferred(), normal, "the backlog stops at its ceiling");
    assert_eq!(m.active_transactions(), normal, "a refused INVITE holds no transaction");
    assert_eq!(m.deferred_refused(false), (flood - normal) as u64);
    assert_eq!(m.deferred_refused(true), 0);
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 100), normal, "only admitted INVITEs draw a 100 Trying");
    let refused = responses_of(&wire, 503);
    assert_eq!(refused.len(), flood - normal, "every INVITE past the ceiling is refused");
    assert!(refused.iter().all(|r| r.raw("Retry-After".into()).next() == Some("5")));

    // A refused INVITE's retransmission is judged afresh: the same 503, still
    // no transaction.
    stack.inject(&new_invite(flood - 1)).await;
    elapse_ms(50).await;
    let again = responses_of(&stack.drain_peer(), 503);
    assert_eq!(again.len(), 1);
    assert_eq!(to_tag_of(&again[0]), to_tag_of(&refused[refused.len() - 1]));
    assert_eq!(stack.txn.metrics().active_transactions(), normal);

    // The callers ACK their 503s; the ACKs open nothing.
    for r in &refused {
        stack.inject(&ack_for(r)).await;
    }
    elapse_ms(50).await;
    assert_eq!(stack.txn.metrics().active_transactions(), normal);

    // The consumer catches up: the admitted INVITEs arrive in order.
    stack.drain_events();
    elapse_ms(200).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    let delivered_ids: Vec<String> =
        delivered.iter().map(|r| r.call_id().as_str().to_string()).collect();
    let admitted_ids: Vec<String> = (0..normal).map(|i| format!("new-{i}@unit")).collect();
    assert_eq!(delivered_ids, admitted_ids, "the deferred INVITEs reach the consumer in order");
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 0);
    reject_and_settle(&mut stack, &delivered).await;
}

/// Emergency INVITEs keep the higher ceiling (RFC 4412 priority): between the
/// two ceilings only a non-emergency INVITE is refused; at the emergency
/// ceiling every new INVITE is.
#[tokio::test(start_paused = true)]
async fn emergency_invites_are_admitted_between_the_ceilings_and_refused_at_the_higher_one() {
    let mut stack = bounded_stack(4, 6).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    for i in 0..4 {
        stack.inject(&new_invite(i)).await;
    }
    elapse_ms(50).await;
    stack.inject(&new_invite(4)).await;
    for i in 0..3 {
        stack.inject(&emergency_invite(i)).await;
    }
    elapse_ms(50).await;

    let m = stack.txn.metrics();
    assert_eq!(m.deferred_refused(false), 1, "a normal INVITE at the normal ceiling");
    assert_eq!(m.deferred_refused(true), 1, "an emergency INVITE at the emergency ceiling");
    assert_eq!(m.event_queue_deferred(), 6);
    assert_eq!(m.active_transactions(), 6);
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 100), 6);
    let refused: Vec<String> =
        responses_of(&wire, 503).iter().map(|r| r.call_id().as_str().to_string()).collect();
    assert_eq!(refused, ["new-4@unit", "sos-2@unit"]);
    for r in responses_of(&wire, 503) {
        stack.inject(&ack_for(&r)).await;
    }

    stack.drain_events();
    elapse_ms(200).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(delivered.len(), 6);
    reject_and_settle(&mut stack, &delivered).await;
}

/// At the ceiling, everything a call already admitted still flows: a
/// retransmitted INVITE replays its 100 Trying, a re-INVITE (To-tag) is
/// admitted, and a CANCEL of a deferred INVITE is answered 200 + 487 with its
/// `Cancelled` deferred behind the INVITE. Only the new call is refused.
#[tokio::test(start_paused = true)]
async fn at_the_ceiling_admitted_calls_are_never_refused() {
    let mut stack = bounded_stack(2, 2).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let invite_a = inbound_request("INVITE", "z9hG4bK-a", "a@unit", None);
    stack.inject(&invite_a).await;
    stack.inject(&inbound_request("INVITE", "z9hG4bK-b", "b@unit", None)).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 2);
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 2, "the backlog is at its ceiling");

    stack.inject(&invite_a).await;
    elapse_ms(50).await;
    let wire = stack.drain_peer();
    assert_eq!((count_responses(&wire, 100), count_responses(&wire, 503)), (1, 0));

    stack.inject(&inbound_request("INVITE", "z9hG4bK-c", "c@unit", Some("dlg-c"))).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the re-INVITE is admitted");

    stack.inject(&inbound_request("CANCEL", "z9hG4bK-a", "a@unit", None)).await;
    elapse_ms(50).await;
    let wire = stack.drain_peer();
    assert_eq!((count_responses(&wire, 200), count_responses(&wire, 487)), (1, 1));
    let terminated = responses_of(&wire, 487);

    stack.inject(&inbound_request("INVITE", "z9hG4bK-d", "d@unit", None)).await;
    elapse_ms(50).await;
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 503), 1, "only the new call is refused");
    let m = stack.txn.metrics();
    assert_eq!(m.deferred_refused(false), 1);
    assert_eq!(m.event_queue_deferred(), 4, "A, B, the re-INVITE and A's Cancelled");
    assert_eq!(m.event_queue_deferrals(EventQueueClass::Cancelled), 1);
    stack.inject(&ack_for(&responses_of(&wire, 503)[0])).await;
    stack.inject(&ack_for(&terminated[0])).await;

    stack.drain_events();
    elapse_ms(200).await;
    let events = stack.drain_events();
    let invites = requests_of(&events, "INVITE");
    let order: Vec<&str> = invites.iter().map(|r| r.call_id().as_str()).collect();
    assert_eq!(order, ["a@unit", "b@unit", "c@unit"]);
    assert!(
        matches!(events.last(), Some(TransactionEvent::Cancelled { call_id, .. }) if call_id == "a@unit"),
        "the Cancelled follows the INVITE it cancels"
    );
    reject_and_settle(&mut stack, &invites[1..]).await;
}

/// The sweep deletes a pre-final INVITE server transaction past its bound
/// (`invite_initial_timeout_ms` + the net), and the INVITE it had deferred
/// leaves with it: a consumer that catches up later never receives a request
/// whose transaction, and caller, are gone.
#[tokio::test(start_paused = true)]
async fn a_swept_invite_transaction_takes_its_deferred_invite_with_it() {
    let mut stack = Stack::build_with_config(
        1,
        1024,
        TransactionConfig {
            udp_queue_max: UDP_QUEUE_MAX,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_initial_timeout_ms: 1_000,
            ..Default::default()
        },
    )
    .await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    stack.inject(&inbound_request("INVITE", "z9hG4bK-stale", "stale@unit", None)).await;
    elapse_ms(50).await;
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 1);

    // Past the pre-final INVITE sweep age (1 s + the 35 s net) and one sweep.
    elapse_ms(50_000).await;
    let m = stack.txn.metrics();
    assert_eq!(m.active_transactions(), 0, "the sweep deleted the transaction");
    assert_eq!(m.event_queue_deferred(), 0, "and the INVITE it deferred");
    assert_eq!(m.deferred_swept(), 1);

    assert_eq!(stack.drain_events().len(), cap);
    elapse_ms(200).await;
    assert!(requests_of(&stack.drain_events(), "INVITE").is_empty());
}
