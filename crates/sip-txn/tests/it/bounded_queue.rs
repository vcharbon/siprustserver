//! Port of `tests/sip/transaction-layer-bounded-queue.test.ts` — the inbound→
//! app event queue is bounded; producers never block; excess is dropped-newest
//! and counted by reason; draining recovers normal accept.
//!
//! Adaptation: the source pumps incrementally so its tiny UDP recv queue
//! (`udpQueueMax`) never fills. Here the b2bua endpoint is bound with a
//! generous recv queue so the network layer never tail-drops, isolating the
//! event-queue bound under test. (The UDP-queue tail-drop is sip-net's own
//! concern, covered by its `simulated` tests.)

use crate::common;
use common::*;
use std::sync::Arc;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::{SipMessage, SipRequest};
use sip_txn::{
    DeferredBound, EventQueueClass, IdGen, InviteClass, InviteRefusals, TransactionConfig,
    TransactionEvent,
};

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
    for class in InviteClass::ALL {
        assert_eq!(m.deferred_refused(class), 0);
    }
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

/// The class of an INVITE: in a dialog when it carries a To-tag, emergency
/// when it carries an emergency priority (RFC 4412).
fn class_of(req: &SipRequest) -> InviteClass {
    if req.to().tag().is_some() {
        InviteClass::InDialog
    } else if sip_message::emergency::is_emergency_request(req) {
        InviteClass::Emergency
    } else {
        InviteClass::Normal
    }
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
            deferred_bound: Some(DeferredBound { normal, emergency, class_of }),
            invite_refusals: Some(InviteRefusals::new(Arc::new(refuse_503))),
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
            &GenerateResponseOpts {
                to_tag: Some(invite.to().tag().map_or_else(|| format!("uas-{i}"), str::to_string)),
                ..Default::default()
            },
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
    assert_eq!(m.deferred_refused(InviteClass::Normal), (flood - normal) as u64);
    assert_eq!(m.deferred_refused(InviteClass::Emergency), 0);
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 100), normal, "only admitted INVITEs draw a 100 Trying");
    let refused = responses_of(&wire, 503);
    assert_eq!(refused.len(), flood - normal, "every INVITE past the ceiling is refused");
    assert!(refused.iter().all(|r| r.raw("Retry-After".into()).next() == Some("5")));

    // A refused INVITE's retransmission draws the same 503, still no
    // transaction, and is not counted as a second refusal.
    stack.inject(&new_invite(flood - 1)).await;
    elapse_ms(50).await;
    let again = responses_of(&stack.drain_peer(), 503);
    assert_eq!(again.len(), 1);
    assert_eq!(to_tag_of(&again[0]), to_tag_of(&refused[refused.len() - 1]));
    assert_eq!(stack.txn.metrics().active_transactions(), normal);
    assert_eq!(stack.txn.metrics().deferred_refused(InviteClass::Normal), (flood - normal) as u64);
    assert_eq!(stack.txn.metrics().refused_copies(), 1, "the copy is counted as a copy");

    // The callers ACK their 503s: the layer that refused absorbs the ACKs, so
    // none opens a transaction or competes for the full queue.
    for r in &refused {
        stack.inject(&ack_for(r)).await;
    }
    elapse_ms(50).await;
    let m = stack.txn.metrics();
    assert_eq!(m.active_transactions(), normal);
    assert_eq!(m.event_queue_drops(EventQueueClass::RequestOther), 0, "no ACK reached the queue");

    // The consumer catches up: the admitted INVITEs arrive in order.
    stack.drain_events();
    elapse_ms(200).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    let delivered_ids: Vec<String> =
        delivered.iter().map(|r| r.call_id().as_str().to_string()).collect();
    let admitted_ids: Vec<String> = (0..normal).map(|i| format!("new-{i}@unit")).collect();
    assert_eq!(delivered_ids, admitted_ids, "the deferred INVITEs reach the consumer in order");
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 0);

    // An ACK copy arriving once the queue has room is absorbed all the same.
    stack.inject(&ack_for(&refused[0])).await;
    elapse_ms(50).await;
    assert!(requests_of(&stack.drain_events(), "ACK").is_empty(), "the consumer never sees it");
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
    assert_eq!(m.deferred_refused(InviteClass::Normal), 1, "a normal INVITE at the normal ceiling");
    assert_eq!(
        m.deferred_refused(InviteClass::Emergency),
        1,
        "an emergency INVITE at the emergency ceiling"
    );
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

/// At the normal ceiling, everything a call already admitted still flows: a
/// retransmitted INVITE replays its 100 Trying, a re-INVITE (To-tag) is
/// admitted below the emergency ceiling, and a CANCEL of a deferred INVITE is
/// answered 200 + 487 with its `Cancelled` deferred behind the INVITE. Only
/// the new call is refused.
#[tokio::test(start_paused = true)]
async fn at_the_ceiling_admitted_calls_are_never_refused() {
    let mut stack = bounded_stack(2, 4).await;
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
    assert_eq!(m.deferred_refused(InviteClass::Normal), 1);
    assert_eq!(m.deferred_refused(InviteClass::InDialog), 0);
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

/// A pre-final INVITE server transaction leaves at its backstop
/// (`invite_initial_timeout_ms` + `TXN_MAX_AGE` from its admission), and the
/// INVITE it had deferred leaves with it: a consumer that catches up later never receives a request
/// whose transaction, and caller, are gone.
#[tokio::test(start_paused = true)]
async fn an_invite_transaction_past_its_backstop_takes_its_deferred_invite_with_it() {
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

    // Past the backstop (1 s + the 35 s margin).
    elapse_ms(50_000).await;
    let m = stack.txn.metrics();
    assert_eq!(m.active_transactions(), 0, "the backstop deleted the transaction");
    assert_eq!(m.sweep_reaped(), 0, "by its own timer");
    assert_eq!(m.event_queue_deferred(), 0, "and the INVITE it deferred");
    assert_eq!(m.deferred_swept(), 1);

    assert_eq!(stack.drain_events().len(), cap);
    elapse_ms(200).await;
    assert!(requests_of(&stack.drain_events(), "INVITE").is_empty());
}

/// An INVITE carrying a To-tag is judged at the emergency ceiling: this layer
/// cannot tell a dialog its consumer holds from one it never had, so a flood
/// of tagged INVITEs stops there too, answered 503, which leaves a real
/// dialog in place (RFC 3261 §12.2.1.2).
#[tokio::test(start_paused = true)]
async fn a_flood_of_to_tagged_invites_stops_at_the_emergency_ceiling() {
    let (normal, emergency, flood) = (2, 4, 10);
    let mut stack = bounded_stack(normal, emergency).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    for i in 0..flood {
        stack
            .inject(&inbound_request(
                "INVITE",
                &format!("z9hG4bK-tagged-{i}"),
                &format!("tagged-{i}@unit"),
                Some(&format!("dlg-{i}")),
            ))
            .await;
    }
    elapse_ms(100).await;

    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_deferred(), emergency, "the backlog stops at the emergency ceiling");
    assert_eq!(m.active_transactions(), emergency);
    assert_eq!(m.deferred_refused(InviteClass::InDialog), (flood - emergency) as u64);
    assert_eq!(m.deferred_refused(InviteClass::Normal), 0);
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 100), emergency);
    let refused = responses_of(&wire, 503);
    assert_eq!(refused.len(), flood - emergency);
    for r in &refused {
        stack.inject(&ack_for(r)).await;
    }

    stack.drain_events();
    elapse_ms(200).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(delivered.len(), emergency);
    reject_and_settle(&mut stack, &delivered).await;
}

/// An emergency ceiling set below the normal one is read as the normal one:
/// an emergency INVITE is never held to a lower ceiling than an ordinary call.
#[tokio::test(start_paused = true)]
async fn an_emergency_ceiling_below_the_normal_one_is_read_as_the_normal_one() {
    let mut stack = bounded_stack(4, 2).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    stack.inject(&new_invite(0)).await;
    stack.inject(&new_invite(1)).await;
    elapse_ms(50).await;
    stack.inject(&emergency_invite(0)).await;
    elapse_ms(50).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 100),
        3,
        "the emergency INVITE is admitted past its own ceiling of 2"
    );
    stack.inject(&new_invite(2)).await;
    stack.inject(&emergency_invite(1)).await;
    elapse_ms(50).await;
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 100), 1);
    let refused = responses_of(&wire, 503);
    assert_eq!(refused.len(), 1);
    assert_eq!(stack.txn.metrics().deferred_refused(InviteClass::Emergency), 1);
    stack.inject(&ack_for(&refused[0])).await;

    stack.drain_events();
    elapse_ms(200).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(delivered.len(), 4);
    reject_and_settle(&mut stack, &delivered).await;
}

/// RFC 3261 §8.2.7: a copy of a refused INVITE that crosses its 503 and
/// arrives after the backlog drained draws the same 503 — admitting it would
/// start a call its caller, already holding a final, never waits for. Past
/// 64·T1, when no UAC still retransmits it, the identity is forgotten.
#[tokio::test(start_paused = true)]
async fn a_copy_crossing_its_refusal_after_the_backlog_drained_draws_the_same_refusal() {
    let mut stack = bounded_stack(2, 4).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    for i in 0..3 {
        stack.inject(&new_invite(i)).await;
    }
    elapse_ms(50).await;
    let refused = responses_of(&stack.drain_peer(), 503);
    assert_eq!(refused.len(), 1);

    stack.drain_events();
    elapse_ms(200).await;
    let mut delivered = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(delivered.len(), 2);
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 0, "the backlog drained");

    stack.inject(&new_invite(2)).await;
    elapse_ms(50).await;
    let wire = stack.drain_peer();
    assert_eq!(count_responses(&wire, 100), 0, "the crossing copy starts no call");
    let again = responses_of(&wire, 503);
    assert_eq!(again.len(), 1);
    assert_eq!(to_tag_of(&again[0]), to_tag_of(&refused[0]));
    let m = stack.txn.metrics();
    assert_eq!(m.active_transactions(), 2);
    assert_eq!(m.deferred_refused(InviteClass::Normal), 1);
    assert_eq!(m.refused_copies(), 1, "the crossing copy is counted as a copy");
    assert!(requests_of(&stack.drain_events(), "INVITE").is_empty());
    stack.inject(&ack_for(&refused[0])).await;

    // 64·T1 later the identity is forgotten: a new INVITE on it is admitted.
    elapse_ms(33_000).await;
    stack.drain_peer();
    stack.inject(&new_invite(2)).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1);
    delivered.extend(requests_of(&stack.drain_events(), "INVITE"));
    assert_eq!(delivered.len(), 3);
    reject_and_settle(&mut stack, &delivered).await;
}

/// An INVITE from `from_tag`: the fixture's request under another From-tag.
fn invite_from(
    method: &str,
    branch: &str,
    call_id: &str,
    from_tag: &str,
    to: Option<&str>,
) -> Vec<u8> {
    String::from_utf8(inbound_request(method, branch, call_id, to))
        .expect("utf-8")
        .replace("tag=caller-tag", &format!("tag={from_tag}"))
        .into_bytes()
}

/// Two INVITEs on one Via branch (RFC 3261 §17.2.3 keys the transaction by
/// branch, sent-by and method; two peers can collide on a branch): the first's
/// transaction ends by its own timers while its INVITE and `Cancelled` still
/// wait, the second is admitted and then left unanswered at its backstop,
/// which takes the second's INVITE only. The second differs from the first in
/// its Call-ID, its From-tag or its top-Via sent-by (`b_sent_by`).
async fn the_backstop_takes_only_the_unanswered_invite_on_a_shared_branch(
    b_call_id: &str,
    b_from: &str,
    b_sent_by: &str,
) {
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

    let branch = "z9hG4bK-shared";
    stack.inject(&invite_from("INVITE", branch, "a@unit", "tag-a", None)).await;
    elapse_ms(50).await;
    stack.inject(&invite_from("CANCEL", branch, "a@unit", "tag-a", None)).await;
    elapse_ms(50).await;
    let terminated = responses_of(&stack.drain_peer(), 487);
    assert_eq!(terminated.len(), 1);
    let to_a = to_tag_of(&terminated[0]);
    stack.inject(&invite_from("ACK", branch, "a@unit", "tag-a", Some(&to_a))).await;
    elapse_ms(6_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "Timer I ended the first");
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 2, "its INVITE and Cancelled wait");

    let second = String::from_utf8(invite_from("INVITE", branch, b_call_id, b_from, None))
        .expect("utf-8")
        .replace("Via: SIP/2.0/UDP 10.0.0.1:5555", &format!("Via: SIP/2.0/UDP {b_sent_by}"));
    stack.inject(second.as_bytes()).await;
    elapse_ms(50).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "the second is admitted");
    assert_eq!(stack.txn.metrics().event_queue_deferred(), 3);

    elapse_ms(50_000).await;
    let m = stack.txn.metrics();
    assert_eq!(m.active_transactions(), 0, "the backstop deleted the second");
    assert_eq!(m.sweep_reaped(), 0, "by its own timer");
    assert_eq!(m.deferred_swept(), 1);
    assert_eq!(m.event_queue_deferred(), 2, "the first's INVITE and Cancelled stay");

    assert_eq!(stack.drain_events().len(), cap);
    elapse_ms(200).await;
    let events = stack.drain_events();
    let invites = requests_of(&events, "INVITE");
    assert_eq!(invites.len(), 1);
    assert_eq!(invites[0].call_id().as_str(), "a@unit");
    assert_eq!(invites[0].from().tag(), Some("tag-a"));
    assert!(matches!(events.last(), Some(TransactionEvent::Cancelled { .. })));
}

#[tokio::test(start_paused = true)]
async fn the_backstop_keys_a_shared_branch_by_call_id() {
    the_backstop_takes_only_the_unanswered_invite_on_a_shared_branch(
        "b@unit",
        "tag-a",
        "10.0.0.1:5555",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_backstop_keys_a_shared_branch_by_from_tag() {
    the_backstop_takes_only_the_unanswered_invite_on_a_shared_branch(
        "a@unit",
        "tag-b",
        "10.0.0.1:5555",
    )
    .await;
}

/// The same identity but for the sent-by: another sender's transaction.
#[tokio::test(start_paused = true)]
async fn the_backstop_keys_a_shared_branch_by_sent_by() {
    the_backstop_takes_only_the_unanswered_invite_on_a_shared_branch(
        "a@unit",
        "tag-a",
        "10.0.0.2:5555",
    )
    .await;
}
