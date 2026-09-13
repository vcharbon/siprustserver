//! S12 tests: the **Forward** apply guard — a forward flush never regresses a
//! backup's progress (ADR-0031 D3).
//!
//! The direction under test is the **Backup flow** (`partition = Bak`): the
//! authority's `Put`/`Delete` landing on a peer that holds its `bak:{primary}`
//! Element. Once an acting backup has bumped `b` on that Element, the
//! authority's own `(p+k, 0)` view is a branch, not a newer version:
//!
//! - a `Put` whose BODY stands behind the Element on the call's lifecycle is a
//!   branch of the call, and is refused; so is one whose `b'` is behind the
//!   Element's `b`;
//! - a `Delete` is refused for the one fact the guard protects — the ANSWER: an
//!   `Active` Element whose caller was answered and for which the authority has
//!   never itself flushed an answer. Everything else takes delete-wins.
//!
//! The Backup flow is Forward in BOTH phases: a bulk re-seed after a
//! `ResetToBootstrap` is a forward flush in bulk, and the guard holds there too.
//! Two directions are pinned unchanged beside it: the **Bootstrap** phase of the
//! Reclaim flow (a node recovering its own partition takes the replica's `(p,b)`
//! as-is, ADR-0014 §3) and the **Reverse** rule's delete-wins.
//!
//! Bodies here are real msgpack `Call`s, because the delete guard reads the
//! stored body's lifecycle state. All run under `#[tokio::test(start_paused =
//! true)]`; the protocol is driven BETWEEN `advance`s and transit delay is
//! `>= 1 ms` (CLAUDE.md fake-clock hazards).

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, CallBodyCodec, CallModelState, LegDisposition, LegState, MsgpackCodec};
use repl_net::transport::SimulatedReplicationNetwork;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::test_support::{fast_config, fwd, one_peer, rev, supervisor_for, tick, Node};
use super::ReplicatingCallStore;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::store::{CallStore, PartitionRole, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9200 + n))
}

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// A proxied INVITE keyed by `cid`, carrying the stickiness cookie so the call
/// it builds is replicable (`topology = {pri, bak}`).
fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// A call `pri` owns and `bak` backs up, in `state` with its a-leg in
/// `a_state`. `a_state == Confirmed` is the caller-answered body an acting
/// backup writes; `Early` the ringing one its primary still holds, whose own
/// teardown ends in the ring deadline's `480`. Whether the caller was answered
/// is a DURABLE fact of the body — the final the a-leg's initial INVITE server
/// transaction took — so it is written here beside the leg state, as the live
/// path writes it.
fn call_in(pri: &str, bak: &str, cid: &str, a_state: LegState, state: CallModelState) -> Call {
    let config = B2buaConfig { self_ordinal: pri.into(), ..Default::default() };
    let mut call = build_initial_call(&invite(pri, bak, cid), src(), &config, 0);
    let answered = a_state == LegState::Confirmed;
    call.a_leg.disposition =
        if answered { LegDisposition::Bridged } else { LegDisposition::Pending };
    call.a_leg.invite_final_sent = match (answered, state) {
        (true, _) => Some(200),
        (false, CallModelState::Active) => None,
        (false, _) => Some(480),
    };
    call.a_leg.state = a_state;
    call.state = state;
    call
}

/// The msgpack body of `call` — what a flush puts on the wire.
fn body(call: &Call) -> Vec<u8> {
    MsgpackCodec::new().encode(call)
}

/// The lifecycle state of the body `node` holds in `(role, primary, call_ref)`,
/// as `(call.state, a_leg.state)`. Panics if nothing is stored.
async fn stored_state(
    store: &ReplicatingCallStore,
    role: PartitionRole,
    primary: &str,
    call_ref: &str,
) -> (CallModelState, LegState) {
    let raw = store.get_call(role, primary, call_ref).await.unwrap().expect("Element present");
    let call: Call = MsgpackCodec::new().decode(&raw).expect("Element body decodes");
    (call.state, call.a_leg.state)
}

/// A → B forward flush at `(p, b)` carrying `call`'s body.
async fn forward(store: &ReplicatingCallStore, call: &Call, p: i64, b: i64) {
    store
        .put_call(PRI, "A", &call.call_ref, body(call), &[], 600_000, p, b, &fwd("B"))
        .await
        .unwrap();
}

/// The `bak:A` Element B holds, written locally at `(p, b)` — what an acting
/// backup's own mutation leaves behind (no propagation: `PutOpts::default()`).
async fn element(store: &ReplicatingCallStore, call: &Call, p: i64, b: i64) {
    element_for(store, call, p, b, 600_000).await;
}

/// The same, with an explicit body TTL — so a test can let the Element lapse.
async fn element_for(store: &ReplicatingCallStore, call: &Call, p: i64, b: i64, ttl_ms: i64) {
    store
        .put_call(BAK, "A", &call.call_ref, body(call), &[], ttl_ms, p, b, &PutOpts::default())
        .await
        .unwrap();
}

/// A (primary) and B (its backup, pulling A's Backup flow), both caught up.
async fn primary_and_backup(
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
    base: u16,
) -> (Node, Node, super::ReplicationSupervisor) {
    let a = Node::spawn("A", addr(base), 1, net, clock).await;
    let b = Node::spawn("B", addr(base + 1), 1, net, clock).await;
    let b_sup =
        supervisor_for("B", &b.store, net, clock, vec![("A".into(), a.addr)], fast_config());
    b_sup.start(one_peer("A", clock));
    tick(200).await;
    (a, b, b_sup)
}

// ---------------------------------------------------------------------------
// FORWARD `Put`: a `b'` behind the Element's `b` is a branch, not a version.
//
// A rings; B holds the ringing Element. B takes the call over and answers the
// caller, bumping its own counter to (1,1). A — partitioned, unaware — tears its
// own unanswered copy down and flushes (2,0). On the heal that Put must NOT land:
// the Element the survivor answered on stays.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_put_behind_the_elements_backup_counter_is_refused() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 1).await;

    let ringing = call_in("A", "B", "s12-put", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();

    // A's ringing version reaches B's `bak:A` at (1,0).
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), Some((1, 0)), "B holds the ringing (1,0)");

    // B takes over and answers the caller: its own counter moves, A's does not.
    let answered = call_in("A", "B", "s12-put", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 1).await;

    // A, alone behind the partition, tears its unanswered copy down and flushes.
    let teardown = call_in("A", "B", "s12-put", LegState::Early, CallModelState::Terminating);
    forward(&a.store, &teardown, 2, 0).await;
    tick(200).await;

    assert_eq!(
        stored_state(&b.store, BAK, "A", &call_ref).await,
        (CallModelState::Active, LegState::Confirmed),
        "the Element keeps the body the backup answered on",
    );
    assert_eq!(
        b.store.current_cv(BAK, "A", &call_ref),
        Some((1, 1)),
        "a forward Put behind the Element's b never lands",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("put"), 1, "counted, by op");
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 0);
}

// ---------------------------------------------------------------------------
// FORWARD `Put`: the refusal is narrow — an equal or advanced `b'` is the
// authority carrying the backup's progress forward, and applies.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_put_at_or_ahead_of_the_elements_backup_counter_applies() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, _b_sup) = primary_and_backup(&net, &clock, 3).await;

    let ringing = call_in("A", "B", "s12-ahead", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;

    let answered = call_in("A", "B", "s12-ahead", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 1).await;

    // The primary folded the backup's `b` and flushes on top of it: `b' == b`.
    let folded = call_in("A", "B", "s12-ahead", LegState::Confirmed, CallModelState::Terminating);
    forward(&a.store, &folded, 2, 1).await;
    tick(200).await;
    assert_eq!(
        stored_state(&b.store, BAK, "A", &call_ref).await,
        (CallModelState::Terminating, LegState::Confirmed),
        "an equal b' is the authority on top of the backup's progress",
    );
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), Some((2, 1)));

    // And a `b'` past the Element's is a version the Element has never seen. The
    // body still moves ALONG the call (the teardown completes); a body that went
    // back down the chain would be a branch whatever the counters said.
    let ahead = call_in("A", "B", "s12-ahead", LegState::Confirmed, CallModelState::Terminated);
    forward(&a.store, &ahead, 3, 2).await;
    tick(200).await;
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), Some((3, 2)), "an advanced b' applies");
}

// ---------------------------------------------------------------------------
// FORWARD `Put`: a body that goes BACK down the call's lifecycle is a branch,
// whatever the counters say. Two live owners exchange versions across a split
// and each keeps adopting the other's axis, so `b'` level with the Element is
// the ordinary case — only the bodies say which of the two views is this call.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_put_whose_body_leaves_the_calls_chain_is_refused_at_a_level_counter() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 19).await;

    let ringing = call_in("A", "B", "s12-branch", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;
    let answered = call_in("A", "B", "s12-branch", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 1).await;

    // A has adopted the Element's `b` but never took its answer: it flushes its
    // own unanswered teardown at a LEVEL `b`.
    let teardown = call_in("A", "B", "s12-branch", LegState::Early, CallModelState::Terminating);
    forward(&a.store, &teardown, 2, 1).await;
    tick(200).await;

    assert_eq!(
        stored_state(&b.store, BAK, "A", &call_ref).await,
        (CallModelState::Active, LegState::Confirmed),
        "an unanswered teardown never overwrites the answer the Element records",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("put"), 1, "counted, by op");

    // And the `Delete` that branch's discharge propagates is refused with it.
    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert!(
        b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_some(),
        "the teardown of a branch evicts nothing",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 1, "counted, by op");
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: delete-wins yields to the ANSWER. An `Active` Element whose
// caller was answered, by an authority whose own view of the call never took
// that answer, is a live call the authority is tearing down because it does not
// know it happened.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_delete_of_an_answered_element_the_authority_never_answered_is_refused() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 5).await;

    let ringing = call_in("A", "B", "s12-del", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;

    let answered = call_in("A", "B", "s12-del", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 1).await;

    // A tears its own unanswered copy down — the flush that shows B the two
    // views have parted — and then discharges and propagates the delete.
    let teardown = call_in("A", "B", "s12-del", LegState::Early, CallModelState::Terminating);
    forward(&a.store, &teardown, 2, 0).await;
    tick(150).await;
    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;

    assert!(
        b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_some(),
        "the authority's delete evicted an Element its backup is still serving",
    );
    assert_eq!(
        stored_state(&b.store, BAK, "A", &call_ref).await,
        (CallModelState::Active, LegState::Confirmed),
        "the Element the backup is still serving survives the authority's delete",
    );
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), Some((1, 1)));
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 1, "counted, by op");
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("put"), 1, "the branch flush too");
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: an authority merely BEHIND on `(p,b)` is not a branch. Two
// live owners run one flush apart for as long as both serve the call, so a
// counter lag says nothing about what happened to it — only a refused body does.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_delete_applies_when_the_authority_is_only_behind_on_the_vector() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 21).await;

    let answered = call_in("A", "B", "s12-del-lag", LegState::Confirmed, CallModelState::Active);
    let call_ref = answered.call_ref.clone();
    forward(&a.store, &answered, 1, 0).await;
    tick(150).await;
    // B serves the call on and its `b` runs ahead of everything A has seen.
    element(&b.store, &answered, 1, 4).await;

    // A ends the call it and B agree on, one `b` behind.
    a.store
        .put_call(PRI, "A", &call_ref, body(&answered), &[], 600_000, 2, 1, &fwd("B"))
        .await
        .unwrap();
    tick(150).await;
    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;

    assert!(
        b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_none(),
        "an authority on the same chain ends the call, however far behind its vector",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 0);
    assert_eq!(
        b_sup.metrics().repl_forward_flush_refused("put"),
        1,
        "its flush was still refused: `b` behind the Element is a version it never saw",
    );
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: delete-wins otherwise. A terminal Element is an ended call
// whichever node ended it; an Element with `b == 0` carries no backup progress
// to protect.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_delete_applies_to_a_terminal_element_and_to_one_without_backup_progress() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, _b_sup) = primary_and_backup(&net, &clock, 7).await;

    // (a) `b > 0`, terminal body → the call is over; the delete lands.
    let ended = call_in("A", "B", "s12-del-term", LegState::Confirmed, CallModelState::Terminated);
    let ended_ref = ended.call_ref.clone();
    forward(&a.store, &ended, 1, 0).await;
    tick(150).await;
    element(&b.store, &ended, 1, 1).await;
    a.store.delete_call(PRI, "A", &ended_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert!(
        b.store.get_call(BAK, "A", &ended_ref).await.unwrap().is_none(),
        "delete-wins over a terminal Element, whatever its b",
    );

    // (b) the Element's answered body came from the authority itself: the call
    // it is ending is the one it knows about.
    let live = call_in("A", "B", "s12-del-b0", LegState::Confirmed, CallModelState::Active);
    let live_ref = live.call_ref.clone();
    forward(&a.store, &live, 1, 0).await;
    tick(150).await;
    assert!(b.store.get_call(BAK, "A", &live_ref).await.unwrap().is_some(), "the Element landed");
    a.store.delete_call(PRI, "A", &live_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert!(
        b.store.get_call(BAK, "A", &live_ref).await.unwrap().is_none(),
        "delete-wins over an Element the authority itself answered on",
    );
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: the authority publishes its answer by FLUSHING it, whether
// or not the Element takes the body. Two live owners of one call refuse each
// other's counters for as long as both serve it, so an authority that answered
// may never land another `Put` — what it flushed is still what it knows.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_refused_flush_that_carries_the_answer_still_frees_the_authoritys_delete() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 29).await;

    let ringing = call_in("A", "B", "s12-del-published", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;

    // B answered the caller and serves it on: its `b` runs away from A's.
    let answered =
        call_in("A", "B", "s12-del-published", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 3).await;

    // A took the answer through the reverse path and flushes it back — refused
    // on the counters, since B has bumped `b` again since A adopted it.
    forward(&a.store, &answered, 2, 1).await;
    tick(200).await;
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), Some((1, 3)), "the counters refused it");
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("put"), 1, "counted, by op");

    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert!(
        b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_none(),
        "a refused flush still showed the Element the authority's own answer",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 0);
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: an ENDING Element takes it. The call is over whoever ended
// it, and whatever record the backup owes rides the reverse path — the guard
// protects a call still being served, not a teardown in flight.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_delete_applies_to_a_terminating_element() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 23).await;

    let ringing = call_in("A", "B", "s12-del-ending", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;

    // The backup answered the caller and is now tearing the call down itself.
    let ending =
        call_in("A", "B", "s12-del-ending", LegState::Confirmed, CallModelState::Terminating);
    element(&b.store, &ending, 1, 1).await;

    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert!(
        b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_none(),
        "an Element that is already ending takes the delete",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 0);
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: an EXPIRED Element takes it. There is no call left to
// protect and no vector to compare — the delete only frees what the lazy TTL
// has already condemned.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_delete_applies_to_an_expired_element() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 25).await;

    let ringing = call_in("A", "B", "s12-del-expired", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;

    // The answered Element the guard would protect — but it lapses first.
    let answered =
        call_in("A", "B", "s12-del-expired", LegState::Confirmed, CallModelState::Active);
    element_for(&b.store, &answered, 1, 1, 1_000).await;
    tick(1_500).await;
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), None, "the Element lapsed");

    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert_eq!(
        b_sup.metrics().repl_forward_flush_refused("delete"),
        0,
        "a lapsed Element has no answer to protect",
    );
    assert!(b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: the guard reads the AUTHORITY's view of the call, and the
// backup's own writes never touch it. An acting backup mutates its Element on
// every turn it serves; if each of those writes reset what the guard reads, the
// authority's delete would land on the next one.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_backups_own_writes_do_not_hand_the_authority_the_delete() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 27).await;

    // A rings; the Element takes A's unanswered view at (1,0).
    let ringing = call_in("A", "B", "s12-del-carry", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;

    // B takes over and answers the caller: (1,1).
    let answered = call_in("A", "B", "s12-del-carry", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 1).await;

    // A, behind the cut, tears its unanswered copy down: refused.
    let teardown = call_in("A", "B", "s12-del-carry", LegState::Early, CallModelState::Terminating);
    forward(&a.store, &teardown, 2, 0).await;
    tick(150).await;
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("put"), 1);

    // B serves the call on — an in-dialog turn of its own, at (1,2).
    element(&b.store, &answered, 1, 2).await;

    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert_eq!(
        stored_state(&b.store, BAK, "A", &call_ref).await,
        (CallModelState::Active, LegState::Confirmed),
        "a write of the backup's own handed the authority the call it never answered",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 1, "counted, by op");
}

// ---------------------------------------------------------------------------
// FORWARD `Delete`: an authority that PUBLISHED THE ANSWER deletes cleanly.
// Once one of its flushes has carried an answered body, the Element's answer is
// the authority's own and its teardown is the end of a call it knows about —
// the ADR-0014 dual-owner trade-off, not a call destroyed behind its owner's
// back.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn forward_delete_applies_once_the_authority_has_taken_the_answer() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, b_sup) = primary_and_backup(&net, &clock, 13).await;

    let ringing = call_in("A", "B", "s12-del-adopted", LegState::Early, CallModelState::Active);
    let call_ref = ringing.call_ref.clone();
    forward(&a.store, &ringing, 1, 0).await;
    tick(150).await;
    let answered =
        call_in("A", "B", "s12-del-adopted", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &answered, 1, 1).await;

    // A folds the backup's flush: it takes the answer and serves the call on.
    forward(&a.store, &answered, 2, 1).await;
    tick(200).await;
    assert_eq!(b.store.current_cv(BAK, "A", &call_ref), Some((2, 1)), "the fold's flush landed");

    // The call ends at A.
    a.store.delete_call(PRI, "A", &call_ref, &[], &fwd("B")).await.unwrap();
    tick(200).await;
    assert!(
        b.store.get_call(BAK, "A", &call_ref).await.unwrap().is_none(),
        "an authority that has taken the answer ends the call",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("delete"), 0);
}

// ---------------------------------------------------------------------------
// FORWARD is the Backup flow in BOTH phases. A `ResetToBootstrap` sends the flow
// back through a bulk re-seed of `bak:{peer}`; those pre-Noop frames are forward
// flushes in bulk, and the guard holds over them exactly as over the tail.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_bulk_re_seed_after_a_reset_still_refuses_a_regressing_put() {
    use std::time::Duration;

    use repl_net::frame::{Frame, Op, Partition, Watermark};
    use repl_net::transport::ReplicationNetwork;

    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    // B holds the Element it authored as the acting backup: answered, (1,1).
    let b = Node::spawn("B", addr(17), 1, &net, &clock).await;
    let answered = call_in("A", "B", "s12-reseed", LegState::Confirmed, CallModelState::Active);
    let call_ref = answered.call_ref.clone();
    element(&b.store, &answered, 1, 1).await;

    // Hand-rolled peer "A". Its Reclaim flow is answered with a bare catch-up
    // Noop (the Backup streams open only once the Reclaim flows are ready).
    // Its Backup flow runs three rounds: 1 = catch-up Noop (the flow bootstraps
    // warm), 2 = ResetToBootstrap, 3 = the bulk re-seed — A's own teardown view
    // at (2,0), the very Put the Forward rule must refuse.
    let a_addr = addr(18);
    let listener = net.listen(a_addr).await.unwrap();
    let regressing =
        body(&call_in("A", "B", "s12-reseed", LegState::Early, CallModelState::Terminating));
    let seed_ref = call_ref.clone();
    let rounds: Arc<std::sync::Mutex<u32>> = Arc::new(std::sync::Mutex::new(0));
    tokio::spawn(async move {
        while let Some(conn) = listener.accept().await {
            let (regressing, seed_ref, rounds) =
                (regressing.clone(), seed_ref.clone(), rounds.clone());
            tokio::spawn(async move {
                let Some(Frame::PullRequest { partition, .. }) = conn.recv().await else {
                    return;
                };
                if partition == Partition::Pri {
                    let _ = conn.send(Frame::Noop { at: Watermark::new(1, 0) }).await;
                    // Hold the Reclaim stream open; it is not what this pins.
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                    return;
                }
                let round = {
                    let mut r = rounds.lock().unwrap();
                    *r += 1;
                    *r
                };
                match round {
                    1 => {
                        let _ = conn.send(Frame::Noop { at: Watermark::new(1, 5) }).await;
                    }
                    2 => {
                        let _ =
                            conn.send(Frame::ResetToBootstrap { reason: "compacted".into() }).await;
                    }
                    _ => {
                        let _ = conn
                            .send(Frame::Data {
                                at: Watermark::new(2, 1),
                                op: Op::Put,
                                partition: Partition::Bak,
                                call_ref: seed_ref,
                                call_gen: 2,
                                call_bgen: 0,
                                body_ttl_ms: 600_000,
                                origin_now_ms: 0,
                                indexes: Vec::new(),
                                body: Some(regressing.into()),
                            })
                            .await;
                        let _ = conn.send(Frame::Noop { at: Watermark::new(2, 1) }).await;
                        // Hold the stream open: the re-seed is the last word.
                        tokio::time::sleep(Duration::from_secs(3_600)).await;
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            });
        }
    });

    let b_sup =
        supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a_addr)], fast_config());
    b_sup.start(one_peer("A", &clock));
    tick(1_500).await;

    assert_eq!(
        stored_state(&b.store, BAK, "A", &call_ref).await,
        (CallModelState::Active, LegState::Confirmed),
        "the bulk re-seed regressed the Element the backup authored",
    );
    assert_eq!(
        b.store.current_cv(BAK, "A", &call_ref),
        Some((1, 1)),
        "a re-seed behind the Element's b is refused like any forward Put",
    );
    assert_eq!(b_sup.metrics().repl_forward_flush_refused("put"), 1, "counted, by op");
}

// ---------------------------------------------------------------------------
// BOOTSTRAP is unchanged (ADR-0014 §3): a node recovering its OWN partition
// takes the replica's `(p,b)` as-is, skipping only a strictly dominated copy.
// The input below is exactly the one Forward refuses — stored `b` ahead of the
// incoming `b'` — and bootstrap must still take it.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn bootstrap_takes_a_replica_the_forward_rule_would_refuse() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(9), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(10), 1, &net, &clock).await;

    // A's own partition carries (1,2); the copy B holds for it carries (2,1) —
    // neither dominates the other, so only the direction decides.
    let mine = call_in("A", "B", "s12-boot", LegState::Early, CallModelState::Active);
    let call_ref = mine.call_ref.clone();
    a.store
        .put_call(PRI, "A", &call_ref, body(&mine), &[], 600_000, 1, 2, &PutOpts::default())
        .await
        .unwrap();
    let theirs = call_in("A", "B", "s12-boot", LegState::Confirmed, CallModelState::Active);
    element(&b.store, &theirs, 2, 1).await;

    // A cold-starts its Reclaim flow from B: every pre-Noop frame is a bootstrap
    // import, not a forward update.
    let a_sup =
        supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a_sup.start(one_peer("B", &clock));
    tick(400).await;

    assert_eq!(
        stored_state(&a.store, PRI, "A", &call_ref).await,
        (CallModelState::Active, LegState::Confirmed),
        "recovery takes the replica as it stands",
    );
    assert_eq!(
        a.store.current_cv(PRI, "A", &call_ref),
        Some((2, 1)),
        "bootstrap takes the replica's (p,b) as-is (ADR-0014 §3)",
    );
    assert_eq!(
        a_sup.metrics().repl_forward_flush_refused("put"),
        0,
        "the guard is Forward-only: bootstrap refuses nothing",
    );
}

// ---------------------------------------------------------------------------
// REVERSE is unchanged: an acting backup's delete for a call this node is
// primary for wins over the primary's own live, answered copy — delete-wins
// holds in the direction the Forward guard does not touch.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn reverse_delete_still_wins_over_the_primarys_own_backup_progress() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(11), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(12), 1, &net, &clock).await;
    // A pulls B, so B's reverse flushes for A's own calls land in A's `pri:A`.
    let a_sup =
        supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a_sup.start(one_peer("B", &clock));
    tick(300).await;

    let answered = call_in("A", "B", "s12-rev-del", LegState::Confirmed, CallModelState::Active);
    let call_ref = answered.call_ref.clone();
    a.store
        .put_call(PRI, "A", &call_ref, body(&answered), &[], 600_000, 1, 1, &PutOpts::default())
        .await
        .unwrap();
    // The backup holds the same call and deletes it toward its primary.
    element(&b.store, &answered, 1, 1).await;
    b.store.delete_call(BAK, "A", &call_ref, &[], &rev("A")).await.unwrap();
    tick(300).await;

    assert!(
        a.store.get_call(PRI, "A", &call_ref).await.unwrap().is_none(),
        "the Reverse direction keeps delete-wins",
    );
}
