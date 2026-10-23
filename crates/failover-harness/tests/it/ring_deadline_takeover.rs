//! **A ring deadline runs from the leg's first 18x across a takeover.** The
//! deadline is armed in the turn that takes the leg's first 180–189 and rides
//! the replicated ledger at its absolute instant, with the leg's "rang" fact:
//!
//! - armed on the primary before it dies, it fires on the survivor at that
//!   instant, and the 18x that hands the call to the survivor does not restart
//!   it (`a_ring_deadline_armed_before_the_kill_…`);
//! - when the primary dies before the callee's first 18x, that 18x reaches the
//!   survivor, which arms the deadline from it (`a_first_18x_reaching_…`);
//! - when the turn that took the first 18x dies with the primary before the
//!   backup holds it, the copy the departure hands the survivor runs the ring
//!   from the callee's INVITE while the leg reads as not having rung
//!   (`a_first_18x_lost_with_the_primary_…`), and an 18x it later takes runs
//!   the ring from that 18x (`a_later_18x_on_the_survivor_…`).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use call::{CallBodyCodec, MsgpackCodec, Ring, TimerType};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::Agent;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The route's ring deadline: not a whole second, and long enough for the
/// kill and the takeover to happen inside it.
const RING_MS: i64 = 6_789;

/// The callee's 18x reaches the worker serving the call two hops after it
/// leaves: callee → front proxy → worker, 1 ms each.
const HOPS_IN_MS: i64 = 2;

/// The ring deadline of the cells whose primary loses bob's 180: the
/// takeover happens inside it, and it ends before the 150 s setup deadline.
const LOST_RING_MS: i64 = 90_123;

/// Every call routes to bob under `ring_ms`; its expiry rejects the caller.
fn ringing_route(ring_ms: i64) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_ms = Some(ring_ms);
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

/// Two replicating workers behind the front proxy, both ready, routing under
/// `ring_ms`.
async fn spawn_cluster(name: &str, ring_ms: i64) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut workers = Vec::new();
    for (ord, addr, peer) in [("b1", B1, "b2"), ("b2", B2, "b1")] {
        workers.push(
            fh.spawn_worker_limited(
                ord,
                ord,
                addr,
                &[peer],
                ("127.0.0.1", 5070),
                ("127.0.0.1", 5080),
                ringing_route(ring_ms),
                Arc::new(NoopLimiter),
            )
            .await,
        );
    }
    let w_b2 = workers.pop().expect("b2");
    let w_b1 = workers.pop().expect("b1");
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");
    Cluster { fh, alice, bob, proxy, w_b1, w_b2 }
}

/// The ring deadline `call` holds, as an absolute instant.
fn ring_deadline(call: &call::Call) -> Option<i64> {
    call.timers.iter().find(|t| *t.timer_type() == TimerType::NoAnswer).map(|t| t.fire_at)
}

/// Advance to within [`QUIET_MARGIN_MS`] of `deadline`: bob is sent no CANCEL
/// before it. The harness advances in 100 ms chunks and one more, so the
/// margin keeps the advance short of the deadline.
async fn quiet_until(fh: &FailoverHarness, bob: &Agent, deadline: i64) {
    const QUIET_MARGIN_MS: i64 = 250;
    let ahead = deadline - QUIET_MARGIN_MS - fh.now_ms();
    fh.advance(Duration::from_millis(u64::try_from(ahead).expect("the deadline is ahead"))).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no CANCEL before the ring deadline"
    );
}

/// Kill the serving node: the proxy reads it dead, the survivor drops it.
async fn kill(
    fh: &FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    proxy: &ProxySut,
) {
    let ord = primary.ordinal().to_string();
    primary.crash();
    proxy.set_health(&ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&ord);
    fh.advance(Duration::from_millis(300)).await;
}

/// Reboot the killed primary empty and let its reclaim run.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    proxy: &ProxySut,
) {
    let ord = primary.ordinal().to_string();
    let new_addr = primary.reboot().await;
    proxy.set_address(&ord, new_addr);
    fh.note_worker_rebound(&ord, new_addr);
    survivor.simulate_peer_added(&ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {ord} became ready");
    proxy.set_health(&ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}

#[tokio::test(start_paused = true)]
async fn a_ring_deadline_armed_before_the_kill_fires_on_the_survivor_at_its_instant() {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } =
        spawn_cluster("ring-deadline-armed-before-the-kill", RING_MS).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    let (primary, survivor) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    fh.advance(Duration::from_millis(700)).await;

    // ── bob rings on the primary: the deadline runs from that 180 ───────────
    let rang = fh.now_ms();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor.scan_one_backed_up(&pri_ord).await.expect("the call replicated");
    let replica = MsgpackCodec::new()
        .decode(
            &survivor
                .get(PartitionRole::Backup, &pri_ord, &call_ref)
                .await
                .expect("the backup holds the replica"),
        )
        .expect("the replica decodes");
    let fire_at = ring_deadline(&replica).expect("the replica carries the ring deadline");
    assert_eq!(fire_at, rang + HOPS_IN_MS + RING_MS, "the deadline runs from the 180's arrival");

    // ── the primary dies; bob's 183 hands the call to the survivor ──────────
    kill(&fh, primary, survivor, &proxy).await;
    uas.respond(183, "Session Progress").await;
    call.expect(183).await;
    fh.advance(Duration::from_millis(100)).await;
    let taken = survivor.live_call(&call_ref).expect("the survivor serves the call");
    assert_eq!(ring_deadline(&taken), Some(fire_at), "a later 18x does not restart the ring");

    // ── the deadline fires on the survivor at its instant ───────────────────
    quiet_until(&fh, &bob, fire_at).await;
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    assert_eq!(fh.now_ms(), fire_at + HOPS_IN_MS, "the CANCEL left the survivor at the deadline");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    call.expect(480).await;

    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, survivor, &proxy).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
}

#[tokio::test(start_paused = true)]
async fn a_first_18x_reaching_the_survivor_arms_the_ring_there() {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } =
        spawn_cluster("ring-deadline-armed-on-the-survivor", RING_MS).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    let (primary, survivor) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor.scan_one_backed_up(&pri_ord).await.expect("the call replicated");

    // ── the primary dies before bob rings ───────────────────────────────────
    kill(&fh, primary, survivor, &proxy).await;
    fh.advance(Duration::from_millis(1_000)).await;

    // ── bob's first 18x reaches the survivor, which arms the ring from it ───
    let rang = fh.now_ms();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let taken = survivor.live_call(&call_ref).expect("the survivor serves the call");
    assert_eq!(
        ring_deadline(&taken),
        Some(rang + HOPS_IN_MS + RING_MS),
        "the survivor runs the ring from the 180 it took"
    );
    assert_eq!(taken.b_legs[0].ring, Ring::Rang, "the 180 is on record");

    quiet_until(&fh, &bob, rang + HOPS_IN_MS + RING_MS).await;
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    assert_eq!(
        fh.now_ms(),
        rang + HOPS_IN_MS + RING_MS + HOPS_IN_MS,
        "the CANCEL left at the deadline"
    );
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    call.expect(480).await;

    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, survivor, &proxy).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
}

/// The callee's 180 dies with the primary: the primary is down before the
/// front proxy reads it dead, so the 180 it forwards there is lost, and the
/// backup holds the call as it was before it — the record a copy reads when
/// the turn that took a 180 died unreplicated. The caller saw no 180, so the
/// caller-facing dialog the lost turn would have opened is not in play here.
/// Returns the call ref, the INVITE's send instant, alice's INVITE and bob's
/// server transaction.
async fn ring_lost_with_the_primary(
    c: &mut Cluster,
    primary_is_b1: &mut bool,
) -> (String, i64, scenario_harness::ClientInvite, scenario_harness::ServerTxn) {
    let call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    *primary_is_b1 = pri_ord == "b1";
    let (primary, backup) =
        if *primary_is_b1 { (&mut c.w_b1, &mut c.w_b2) } else { (&mut c.w_b2, &mut c.w_b1) };
    c.fh.advance(Duration::from_millis(500)).await;
    let call_ref = backup.scan_one_backed_up(&pri_ord).await.expect("the call replicated");
    let replica = MsgpackCodec::new()
        .decode(
            &backup
                .get(PartitionRole::Backup, &pri_ord, &call_ref)
                .await
                .expect("the backup holds the call"),
        )
        .expect("the replica decodes");
    let sent_at = replica.b_legs[0].outbound_invite.as_ref().expect("dialled").sent_at_ms;
    assert_eq!(ring_deadline(&replica), None, "no ring deadline before the 180");

    // ── the primary dies; bob's 180 is forwarded to it and lost ─────────────
    primary.crash();
    uas.respond(180, "Ringing").await;
    c.fh.advance(Duration::from_millis(100)).await;
    c.proxy.set_health(&pri_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&pri_ord);
    c.fh.advance(Duration::from_millis(300)).await;
    (call_ref, sent_at, call, uas)
}

/// Bob's 180 dies with the primary, whose departure hands the call to the
/// survivor: the copy it takes over shows no ring, so it runs bob's ring from
/// his INVITE and gives the ring up `no_answer_ms` after that INVITE —
/// not at the setup deadline. Its INVITE transaction holds no provisional on
/// record, so the CANCEL waits for one (RFC 3261 §9.1) and has the INVITE
/// re-sent once; bob's transaction answers that retransmission with his latest
/// provisional (§17.2.1), whose Record-Route brings it to the survivor and
/// releases the CANCEL; bob's INVITE ends 487.
#[tokio::test(start_paused = true)]
async fn a_first_18x_lost_with_the_primary_presumes_the_ring_from_the_invite() {
    let mut c = spawn_cluster("ring-deadline-lost-18x-presumed", LOST_RING_MS).await;
    let mut primary_is_b1 = true;
    let (call_ref, sent_at, mut call, mut uas) =
        ring_lost_with_the_primary(&mut c, &mut primary_is_b1).await;
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = c;
    let (primary, survivor) =
        if primary_is_b1 { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    let taken = survivor.live_call(&call_ref).expect("the departure handed the call over");
    assert_eq!(taken.b_legs[0].ring, Ring::Silent, "no 18x of bob's is on record");
    let fire_at = sent_at + LOST_RING_MS;
    assert_eq!(ring_deadline(&taken), Some(fire_at), "the ring runs from the INVITE");

    quiet_until(&fh, &bob, fire_at).await;
    call.expect(480).await;
    assert_eq!(
        fh.now_ms(),
        fire_at + HOPS_IN_MS,
        "the caller is answered at the presumed deadline"
    );

    let resent = bob.receive("INVITE").await;
    assert_eq!(resent.request().top_via().branch(), uas.request().top_via().branch());
    assert!(bob.try_receive_tolerating("CANCEL", &[]).await.is_none(), "the CANCEL is held");
    uas.respond(180, "Ringing").await;
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    fh.advance(Duration::from_secs(40)).await;
    assert!(bob.try_receive_tolerating("INVITE", &[]).await.is_none(), "re-sent once only");

    reboot_and_reclaim(&mut fh, primary, survivor, &proxy).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
}

/// Same lost 180; bob's 183 then hands the call to the survivor, which first
/// presumes bob ringing since his INVITE and then, taking that 183, runs his
/// ring from it.
#[tokio::test(start_paused = true)]
async fn a_later_18x_on_the_survivor_replaces_the_presumed_ring() {
    let mut c = spawn_cluster("ring-deadline-lost-18x-takeover", LOST_RING_MS).await;
    let mut primary_is_b1 = true;
    let (call_ref, _sent_at, mut call, mut uas) =
        ring_lost_with_the_primary(&mut c, &mut primary_is_b1).await;
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = c;
    let (primary, survivor) =
        if primary_is_b1 { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    fh.advance(Duration::from_millis(2_000)).await;
    let rang = fh.now_ms();
    uas.respond(183, "Session Progress").await;
    call.expect(183).await;
    let taken = survivor.live_call(&call_ref).expect("the survivor serves the call");
    assert_eq!(taken.b_legs[0].ring, Ring::Rang, "the 183 is on record");
    let fire_at = rang + HOPS_IN_MS + LOST_RING_MS;
    assert_eq!(ring_deadline(&taken), Some(fire_at), "the ring runs from the 183");

    quiet_until(&fh, &bob, fire_at).await;
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    assert_eq!(fh.now_ms(), fire_at + HOPS_IN_MS, "the CANCEL left at the deadline");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    call.expect(480).await;

    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, survivor, &proxy).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
}
