//! S13 tests: successive calls on one callRef. A caller retrying after a
//! 401/407/422 or a 3xx re-sends the INVITE with the same Call-ID and From tag,
//! so the retry is born on the callRef of the call it retries, its `(p,b)`
//! restarted at `(1,0)`. Each write names its call incarnation; these scenarios
//! pin what a node does with a write of a call other than the one it holds or
//! buried (ADR-0014, "`(p,b)` orders one call incarnation").
//!
//! Timing is the axis of every scenario: when the late write of the earlier
//! call lands — in the Reclaim flow's bootstrap or in its tail, before or after
//! the new call's `Put` reached the backup. A primary that deletes a call and
//! writes the retry inside one poll drains one changelog entry, the retry's
//! `Put`, so the backup never sees that delete. All run under
//! `#[tokio::test(start_paused = true)]`, the protocol driven between `tick`s.

use std::net::SocketAddr;
use std::sync::Arc;

use call::{
    Call, CallBodyCodec, CallLimiterState, CallModelState, LegDisposition, LegState, LimiterEntry,
    MsgpackCodec,
};
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
    SocketAddr::from(([127, 0, 0, 1], 9300 + n))
}

/// A proxied INVITE on one identity, carrying the stickiness cookie so the
/// call it builds is replicable.
fn invite(cid: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri=A;w_bak=B;e=0;kid=k1;sig=abc;lr>\r\n\
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

/// The `n`th call on identity `cid`, in `state`: its own incarnation, holding
/// one limiter entry under it. `Terminated` is a call ended unanswered.
fn nth(cid: &str, n: u32, state: CallModelState) -> Call {
    let config = B2buaConfig { self_ordinal: "A".into(), ..Default::default() };
    let mut call = build_initial_call(&invite(cid), src(), &config, &sip_txn::IdGen::seeded(1), 0);
    let key = format!("{}#n{n}", call.call_ref);
    call.limiter =
        CallLimiterState::admitted(key, 1, vec![LimiterEntry { id: "x".into(), limit: 10 }]);
    call.state = state;
    if state != CallModelState::Active {
        call.a_leg.state = LegState::Terminated;
        call.a_leg.disposition = LegDisposition::Pending;
        call.a_leg.invite_final_sent = Some(401);
    }
    call
}

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// `opts` naming `call`'s incarnation, as every write of a live call does.
fn named(call: &Call, opts: PutOpts) -> PutOpts {
    PutOpts { incarnation: Some(call.incarnation().to_string()), ..opts }
}

/// The primary's own write of `call` at `(p, 0)`, flushed toward B.
async fn primary_put(a: &ReplicatingCallStore, call: &Call, p: i64) {
    let body = MsgpackCodec::new().encode(call);
    a.put_call(PRI, "A", &call.call_ref, body, &[], 600_000, p, 0, &named(call, fwd("B")))
        .await
        .unwrap();
}

/// The primary removes `call` (its release), propagating the delete to B.
async fn primary_delete(a: &ReplicatingCallStore, call: &Call) {
    a.delete_call(PRI, "A", &call.call_ref, &[], false, &named(call, fwd("B"))).await.unwrap();
}

/// B's own write of `call` at `(p, b)`: what its takeover copy flushes back
/// toward the primary (`b > 0`).
async fn takeover_write(b: &ReplicatingCallStore, call: &Call, p: i64, bgen: i64) {
    let body = MsgpackCodec::new().encode(call);
    b.put_call(BAK, "A", &call.call_ref, body, &[], 30_000, p, bgen, &named(call, rev("A")))
        .await
        .unwrap();
}

/// B's copy of a call A replicated: what B's Backup flow applies.
async fn replicated(b: &ReplicatingCallStore, call: &Call, p: i64) {
    let body = MsgpackCodec::new().encode(call);
    let opts = named(call, PutOpts { origin_now_ms: Some(0), ..PutOpts::default() });
    b.put_call(BAK, "A", &call.call_ref, body, &[], 600_000, p, 0, &opts).await.unwrap();
}

/// The incarnation of the body `store` holds for `call_ref` in `role`, decoded.
async fn held(store: &ReplicatingCallStore, role: PartitionRole, call_ref: &str) -> Option<String> {
    let raw = store.get_call(role, "A", call_ref).await.unwrap()?;
    let call: Call = MsgpackCodec::new().decode(&raw).expect("the body decodes");
    Some(call.incarnation().to_string())
}

/// When the takeover copy's late write of the first call reaches its primary.
#[derive(Clone, Copy, Debug)]
enum LateReverse {
    /// The primary's Reclaim flow is down until after the retries: the write
    /// lands in its bootstrap.
    InTheBootstrap,
    /// The flow streams throughout: the write lands in its tail.
    InTheTail,
}

/// **Two retries on one identity bury both calls.** The first call is
/// challenged (401), the retry is challenged again (407) and abandoned. The
/// backup's takeover copy of the FIRST call flushes its terminal late; the
/// primary deleted that call two deletes ago, and a late write of it must not
/// re-create it — a re-created deferred terminal is reclaimed and discharged a
/// second time.
async fn a_late_reverse_flush_of_the_first_of_two_challenged_calls(at: LateReverse, base: u16) {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(base), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(base + 1), 1, &net, &clock).await;
    let a_sup =
        supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    if let LateReverse::InTheTail = at {
        a_sup.start(one_peer("B", &clock));
        tick(300).await;
    }

    let first = nth("s13-two", 1, CallModelState::Active);
    let call_ref = first.call_ref.clone();
    primary_put(&a.store, &first, 2).await;
    replicated(&b.store, &first, 2).await;
    if let LateReverse::InTheBootstrap = at {
        // B's takeover copy ended the first call before the flow comes up.
        takeover_write(&b.store, &nth("s13-two", 1, CallModelState::Terminated), 2, 1).await;
    }

    // The first call is challenged and released; its retry too.
    primary_delete(&a.store, &first).await;
    let second = nth("s13-two", 2, CallModelState::Active);
    primary_put(&a.store, &second, 1).await;
    tick(100).await;
    primary_delete(&a.store, &second).await;
    tick(100).await;

    match at {
        LateReverse::InTheBootstrap => {
            a_sup.start(one_peer("B", &clock));
        }
        LateReverse::InTheTail => {
            takeover_write(&b.store, &nth("s13-two", 1, CallModelState::Terminated), 2, 1).await;
        }
    }
    tick(400).await;

    assert_eq!(
        held(&a.store, PRI, &call_ref).await,
        None,
        "{at:?}: the first call stays buried after the second one's delete"
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_reverse_flush_of_the_first_of_two_challenged_calls_in_the_bootstrap() {
    a_late_reverse_flush_of_the_first_of_two_challenged_calls(LateReverse::InTheBootstrap, 1).await;
}

#[tokio::test(start_paused = true)]
async fn a_late_reverse_flush_of_the_first_of_two_challenged_calls_in_the_tail() {
    a_late_reverse_flush_of_the_first_of_two_challenged_calls(LateReverse::InTheTail, 3).await;
}

/// When the backup's takeover copy of the first call writes, relative to the
/// retry's `Put` reaching the backup.
#[derive(Clone, Copy, Debug)]
enum TakeoverWrite {
    BeforeTheRetryLands,
    AfterTheRetryLands,
}

/// A and B both up, B pulling A's Backup flow.
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

/// **A takeover copy of a replaced call never overwrites the retry.** B holds
/// the first call and serves a takeover copy of it; A releases the first call
/// and answers the retry inside one poll, so B gets the retry's `Put` and never
/// the first call's delete. Whenever the takeover copy writes, B ends holding
/// the retry: a takeover of the retry after A crashes must read the retry.
async fn a_takeover_write_of_a_replaced_call(at: TakeoverWrite, base: u16) {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, _b_sup) = primary_and_backup(&net, &clock, base).await;

    let first = nth("s13-takeover", 1, CallModelState::Active);
    let call_ref = first.call_ref.clone();
    primary_put(&a.store, &first, 3).await;
    tick(150).await;
    assert_eq!(held(&b.store, BAK, &call_ref).await.as_deref(), Some(first.incarnation()));

    if let TakeoverWrite::BeforeTheRetryLands = at {
        takeover_write(&b.store, &first, 3, 1).await;
    }
    // Inside one poll: the delete and the retry's first write share an entry.
    primary_delete(&a.store, &first).await;
    let retry = nth("s13-takeover", 2, CallModelState::Active);
    primary_put(&a.store, &retry, 1).await;
    tick(200).await;
    if let TakeoverWrite::AfterTheRetryLands = at {
        takeover_write(&b.store, &first, 3, 1).await;
        tick(200).await;
    }

    assert_eq!(
        held(&b.store, BAK, &call_ref).await.as_deref(),
        Some(retry.incarnation()),
        "{at:?}: the backup holds the retry"
    );
}

#[tokio::test(start_paused = true)]
async fn a_takeover_write_before_the_retry_lands_does_not_survive_it() {
    a_takeover_write_of_a_replaced_call(TakeoverWrite::BeforeTheRetryLands, 5).await;
}

#[tokio::test(start_paused = true)]
async fn a_takeover_write_after_the_retry_lands_never_overwrites_it() {
    a_takeover_write_of_a_replaced_call(TakeoverWrite::AfterTheRetryLands, 7).await;
}

/// **A deferred terminal the retry replaces is settled, once.** B's takeover
/// copy ended the first call and deferred its terminal to A; A never reclaims
/// it (A answered the retry instead), and the retry's `Put` replaces it on B.
/// The replaced terminal is handed to the replica reap like an expired one —
/// its own limiter key released, its CDR counted lost — and only once, whether
/// the terminal was written before the retry landed or after.
async fn a_deferred_terminal_the_retry_replaces(at: TakeoverWrite, base: u16) {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b, _b_sup) = primary_and_backup(&net, &clock, base).await;

    let first = nth("s13-deferred", 1, CallModelState::Active);
    let ended = nth("s13-deferred", 1, CallModelState::Terminated);
    primary_put(&a.store, &first, 3).await;
    tick(150).await;
    if let TakeoverWrite::BeforeTheRetryLands = at {
        takeover_write(&b.store, &ended, 3, 1).await;
    }
    primary_delete(&a.store, &first).await;
    let retry = nth("s13-deferred", 2, CallModelState::Active);
    primary_put(&a.store, &retry, 1).await;
    tick(200).await;
    if let TakeoverWrite::AfterTheRetryLands = at {
        takeover_write(&b.store, &ended, 3, 1).await;
    }

    let handed: Vec<String> = b
        .store
        .take_displaced()
        .iter()
        .map(|raw| MsgpackCodec::new().decode(raw).expect("the body decodes"))
        .filter(|c: &Call| c.state == CallModelState::Terminated)
        .map(|c| c.incarnation().to_string())
        .collect();
    assert_eq!(handed, vec![ended.incarnation().to_string()], "{at:?}: the terminal, once");
    assert!(b.store.take_displaced().is_empty(), "{at:?}: and not again");
}

#[tokio::test(start_paused = true)]
async fn a_deferred_terminal_written_before_the_retry_lands_is_settled() {
    a_deferred_terminal_the_retry_replaces(TakeoverWrite::BeforeTheRetryLands, 9).await;
}

#[tokio::test(start_paused = true)]
async fn a_deferred_terminal_written_after_the_retry_lands_is_settled() {
    a_deferred_terminal_the_retry_replaces(TakeoverWrite::AfterTheRetryLands, 11).await;
}

/// **A delete names the call it ends.** B's delete of the first call reaches
/// A, which already holds the retry: A keeps the retry and buries only the
/// first call.
#[tokio::test(start_paused = true)]
async fn a_delete_of_a_replaced_call_leaves_the_retry() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(13), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(14), 1, &net, &clock).await;
    let a_sup =
        supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a_sup.start(one_peer("B", &clock));
    tick(300).await;

    let first = nth("s13-delete", 1, CallModelState::Active);
    let call_ref = first.call_ref.clone();
    replicated(&b.store, &first, 2).await;
    let retry = nth("s13-delete", 2, CallModelState::Active);
    primary_put(&a.store, &retry, 1).await;

    b.store.delete_call(BAK, "A", &call_ref, &[], false, &rev("A")).await.unwrap();
    tick(300).await;

    assert_eq!(
        held(&a.store, PRI, &call_ref).await.as_deref(),
        Some(retry.incarnation()),
        "the delete of the first call leaves the retry"
    );
    assert!(a.store.buries(&call_ref, Some(first.incarnation())), "and buries the first call");
}

/// **A replaced call a takeover copy keeps writing stays buried.** B's takeover
/// copy of the first call outlives the burial window the retry's replacement
/// started, and A has since released the retry: each refused write re-stamps
/// the burial, so the copy's next write is still refused and never pushed back.
#[tokio::test(start_paused = true)]
async fn a_replaced_call_still_written_stays_buried_past_its_first_window() {
    let clock = Clock::test_at(0);
    let b = ReplicatingCallStore::new(1, clock.clone());
    let first = nth("s13-long", 1, CallModelState::Active);
    let retry = nth("s13-long", 2, CallModelState::Active);
    let call_ref = first.call_ref.clone();
    replicated(&b, &first, 3).await;
    replicated(&b, &retry, 1).await;

    let step = std::time::Duration::from_millis(200_000);
    tokio::time::advance(step).await;
    takeover_write(&b, &first, 3, 1).await;
    b.delete_call(BAK, "A", &call_ref, &[], false, &named(&retry, PutOpts::default()))
        .await
        .unwrap();
    tokio::time::advance(step).await;
    takeover_write(&b, &first, 3, 2).await;

    assert_eq!(held(&b, BAK, &call_ref).await, None, "the replaced call stays buried");
    assert_eq!(b.changelog().head().counter, 0, "nothing was pushed back");
}

/// **The authority's own terminal is not a deferred one.** B holds the first
/// call's terminal as A flushed it (`b == 0`): A discharged it itself, and its
/// delete compacted into the retry's `Put`. The replacement hands nothing to
/// the reap, so no CDR A wrote is counted lost.
#[tokio::test(start_paused = true)]
async fn a_replaced_terminal_the_authority_flushed_is_not_counted_lost() {
    let b = ReplicatingCallStore::new(1, Clock::test_at(0));
    let ended = nth("s13-own", 1, CallModelState::Terminated);
    replicated(&b, &ended, 4).await;
    replicated(&b, &nth("s13-own", 2, CallModelState::Active), 1).await;
    assert!(b.take_displaced().is_empty());
}

/// **A release with no resident copy leaves the store alone.** The store
/// holds the retry; a release of the ref finds no call in memory (the call it
/// was meant for is gone), so its delete would name no call and must not
/// remove the retry.
#[tokio::test(start_paused = true)]
async fn a_release_with_no_resident_copy_leaves_the_retry_in_the_store() {
    use crate::metrics::B2buaMetrics;
    use crate::store::{BufferedTerminateWriter, CallState};

    let store = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let writer = BufferedTerminateWriter::spawn(store.clone() as Arc<dyn CallStore>, 16);
    let state = CallState::new(store.clone() as Arc<dyn CallStore>, "A", B2buaMetrics::new())
        .with_replication(store.clone(), writer);
    let retry = nth("s13-release", 2, CallModelState::Active);
    let call_ref = retry.call_ref.clone();
    primary_put(&store, &retry, 1).await;

    state.remove(&call_ref);
    tick(100).await;

    assert_eq!(held(&store, PRI, &call_ref).await.as_deref(), Some(retry.incarnation()));
}
