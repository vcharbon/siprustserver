//! **A worker whose process dies on a live node.** The orchestrator flips its
//! endpoint not ready, not terminating, and it stays in the slice (ADR-0031
//! case 4). The proxy drops it from its projection, usually before its probe
//! reads it dead, and tells that flip as its dead verdict (ADR-0050 §3): the
//! backup confirms it on its own replication stream (ADR-0047 D1) and takes the
//! calls with work in flight over.
//!
//! ```text
//!   alice ── proxy ──┬── b1 primary (process dies, endpoint stays in the slice)
//!                    └── b2 backup  (holds the replica)
//!   b1 not ready, not terminating → proxy tells b2 → b2's connect refused → takeover
//! ```
//!
//! Timing is the axis of every cell here:
//!
//! - the flip against the crash and the probe's verdict ([`Flip`]);
//! - the worker's return against the verdict and its confirmation (the
//!   `returns_*` cells): before the flip, during the confirmation, after the
//!   takeover;
//! - a live worker's flip, refuted by its stream, then its death;
//! - a second proxy's flip, at the same instant or later ([`SecondFlip`]);
//! - a crash loop, the second death before and after the proxy read it alive.
//!
//! Beyond the failover sweep: the endpoint's ready condition as the dead
//! verdict's source, with the flip's timing as its axis.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::{FailoverHarness, ProxySut, WorkerHealth};
use sip_proxy::health::HealthProbeConfig;

use crate::departure_support::{bring_up_deciding, placed, CountedDecision, B1, B2};
use crate::inflight_resend::{settle_to_one_cdr, split_mut, survivor_of};
use crate::ringing_call_taken_over_on_departure::{
    final_at, place, place_ringing_for, replicated_deadline, ring_until, Callee, Placed, OFFER,
};
use crate::swept_call_primary_returns::{
    answer, hang_up_and_settle, hang_up_quiet_and_settle, port, sent,
};
use crate::undecided_call_settled_on_departure::{answer_and_hang_up, dialled, rejections_to};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// A second front proxy, which reads the same endpoint conditions.
const SECOND_PROXY: &str = "127.0.0.1:5081";

/// How long the orchestrator takes to flip a dead process's endpoint: ahead of
/// the probe's earliest verdict.
const FLIP_AFTER_THE_CRASH: Duration = Duration::from_millis(1_500);
/// How soon after a flip a process that refuses the connect is taken over.
const TAKEOVER_BOUND: Duration = Duration::from_millis(500);
/// Past every confirmation of a live stream: its idle bound and one connect.
const CONFIRMED_BY_NOW: Duration = Duration::from_secs(5);
/// Inside the connect bound (ADR-0045): the confirmation still waits.
const INSIDE_THE_CONNECT_BOUND: Duration = Duration::from_millis(800);
/// A ring deadline that outlasts a return and a second death.
const LONG_RING: Duration = Duration::from_secs(40);
/// Just after the backup's puller failed a connect, 800 ms before its next.
const INSIDE_THE_BACKOFF: Duration = Duration::from_millis(750);
/// Long enough for the backup's puller to back off at its ceiling.
const LONG_ABSENCE: Duration = Duration::from_secs(13);
/// How soon the reclaim of a returned worker reaches the backup: the pullers'
/// backoff ceiling, one connect and its first frames.
const STANDS_DOWN_WITHIN: Duration = Duration::from_millis(2_500);
/// How long the routing decision takes in the undecided cells.
const DECISION_DELAY: Duration = Duration::from_secs(3);
/// When the worker returns inside the copy's asked-again decision.
const RETURNS_INSIDE_THE_DECISION: Duration = Duration::from_secs(1);

/// The latest verdict of the default probe.
fn probe_reads_dead_after() -> Duration {
    HealthProbeConfig::default().verdict_within()
}

/// When the endpoint flips against the crash and the probe's verdict.
#[derive(Clone, Copy, Debug)]
enum Flip {
    /// The process dies, the endpoint flips, the probe never gets to read it.
    AfterTheCrash,
    /// The readiness fails with the process still up, and it dies at once.
    JustBeforeTheCrash,
    /// The probe read it dead and told first; the flip adds no verdict.
    AfterTheProbesVerdict,
}

/// `b1` dies and its endpoint flips at `flip`'s timing.
async fn dies_on_a_live_node(p: &mut Placed, flip: Flip) {
    match flip {
        Flip::AfterTheCrash => {
            p.fh.mark("b1", None, "crash", "the process dies, its node lives");
            p.w_b1.crash();
            p.fh.advance(FLIP_AFTER_THE_CRASH).await;
            p.fh.flap_not_ready("b1");
        }
        Flip::JustBeforeTheCrash => {
            p.fh.flap_not_ready("b1");
            p.fh.mark("b1", None, "crash", "the process dies, its node lives");
            p.w_b1.crash();
        }
        Flip::AfterTheProbesVerdict => {
            p.fh.mark("b1", None, "crash", "the process dies, its node lives");
            p.w_b1.crash();
            p.fh.advance(probe_reads_dead_after()).await;
            p.fh.mark("b1", None, "health", "the proxy's probe reads it dead");
            p.proxy.set_health("b1", WorkerHealth::Dead);
            p.fh.advance(TAKEOVER_BOUND).await;
            p.fh.flap_not_ready("b1");
        }
    }
    p.fh.advance(TAKEOVER_BOUND).await;
}

/// The established call ends with the caller's BYE, which reaches the backup;
/// the endpoint then leaves the slice and every call settles to one CDR.
async fn hang_up_and_depart(mut p: Placed) {
    let mut bye = p.quiet.bye().await;
    p.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    p.fh.depart("b1");
    p.fh.advance(Duration::from_secs(10)).await;
    for call_ref in [&p.ringing_ref, &p.quiet_ref] {
        settle_to_one_cdr(&p.fh, &[&p.alice, &p.bob], &[&p.w_b1, &p.w_b2], call_ref).await;
    }
    drop(p.proxy);
}

/// The ringing call runs to the deadline on the copy, the worker staying away.
async fn ends_at_the_deadline_on_the_copy(p: &mut Placed, deadline: i64) {
    ring_until(p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(p, deadline).await;
    assert_eq!(sent(p, p.bob.addr(), b"CANCEL"), [port(B2)], "the backup CANCELs the callee");
    assert_eq!(
        sent(p, p.alice.addr(), b"SIP/2.0 480"),
        [port(B2)],
        "the backup refuses the caller"
    );
}

/// The worker dies at `flip`'s timing and stays away: its backup takes the
/// ringing call over on one verdict and ends it at the replicated deadline.
async fn a_ringing_call_ends_at_its_deadline(name: &str, flip: Flip) {
    let mut p = place(name, Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    dies_on_a_live_node(&mut p, flip).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "one verdict is told");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "its stream confirms it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 1, "the ringing call alone");
    assert!(p.w_b2.serves(&p.ringing_ref), "the backup serves the ringing call");
    assert!(!p.w_b2.serves(&p.quiet_ref), "the established call stays reactive");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

#[tokio::test(start_paused = true)]
async fn a_flip_after_the_crash_and_before_the_probe_is_the_verdict() {
    a_ringing_call_ends_at_its_deadline("not-ready-flip-after-the-crash", Flip::AfterTheCrash)
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_flip_just_before_the_crash_is_confirmed_at_the_streams_end() {
    a_ringing_call_ends_at_its_deadline(
        "not-ready-flip-before-the-crash",
        Flip::JustBeforeTheCrash,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_flip_after_the_probes_verdict_adds_no_second_verdict() {
    a_ringing_call_ends_at_its_deadline(
        "not-ready-flip-after-the-probe",
        Flip::AfterTheProbesVerdict,
    )
    .await;
}

/// A call still undecided at the crash: the flip's verdict has its decision
/// asked again, the callee dialled once, the caller answered, one CDR.
#[tokio::test(start_paused = true)]
async fn an_undecided_call_is_asked_again_on_the_flip() {
    let name = "not-ready-flip-undecided";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "the process dies, its node lives");
    primary.crash();
    fh.advance(FLIP_AFTER_THE_CRASH).await;
    fh.flap_not_ready(&pri_ord);
    fh.advance(TAKEOVER_BOUND).await;
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the backup asks the decision again");

    answer_and_hang_up(&mut call, &bob).await;
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    assert_eq!(dialled(&fh, &bob), 1, "the callee is dialled once");

    fh.depart(&pri_ord);
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// `b1` restarts in place: a higher generation at a new address, its own
/// informer showing its endpoint not ready until its readiness passes.
async fn restarts_in_place(p: &mut Placed) -> SocketAddr {
    p.fh.mark("b1", None, "reboot", "the container restarts in place");
    let addr = p.w_b1.reboot().await;
    p.fh.note_worker_rebound("b1", addr);
    addr
}

/// `b1`'s readiness passes: its endpoint reads ready and the proxy reads it
/// alive at `addr`.
async fn routable_once_ready(p: &mut Placed, addr: SocketAddr) {
    let ready = p
        .fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || p.w_b1.is_ready())
        .await;
    assert!(ready, "the restarted worker became ready");
    p.fh.flap_ready("b1");
    p.proxy.set_address("b1", addr);
    p.proxy.set_health("b1", WorkerHealth::Alive);
}

/// The returned primary alone ends the ringing call at its deadline: one
/// CANCEL, its 487 ACKed, one 480.
async fn ends_at_the_deadline_on_the_primary(p: &mut Placed, deadline: i64) {
    ring_until(p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(p, deadline).await;
    p.fh.advance(Duration::from_secs(40)).await;
    assert_eq!(sent(p, p.bob.addr(), b"CANCEL"), [port(B1)], "the primary alone CANCELs");
    assert_eq!(sent(p, p.alice.addr(), b"SIP/2.0 480"), [port(B1)], "the primary alone refuses");
}

/// Stand-down after the takeover: the copy is live when the worker returns,
/// and stands down on its reclaim write; the primary ends the call alone.
#[tokio::test(start_paused = true)]
async fn returns_after_the_takeover_and_the_copy_stands_down() {
    let mut p = place("not-ready-flip-returns-after", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    dies_on_a_live_node(&mut p, Flip::AfterTheCrash).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "the backup took the call over");

    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    routable_once_ready(&mut p, addr).await;
    p.fh.advance(STANDS_DOWN_WITHIN).await;
    assert_eq!(p.w_b2.metrics().repl_stand_down_total(), 1, "the copy stood down");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy serves nothing now");
    assert!(p.w_b1.serves(&p.ringing_ref), "the returned worker serves its call");
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "the repeated flip tells nothing");

    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The worker stays away for many connects of the backup's puller, then
/// returns: the copy stands down on the reclaim at the puller's next connect,
/// its backoff at its ceiling, and the primary alone ends the call.
#[tokio::test(start_paused = true)]
async fn returns_after_a_long_absence_and_the_copy_stands_down_within_the_backoff() {
    let mut p =
        place_ringing_for("not-ready-flip-returns-long-after", Callee::Ringing, LONG_RING).await;
    let deadline = replicated_deadline(&p).await;
    dies_on_a_live_node(&mut p, Flip::AfterTheCrash).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "the backup took the call over");
    p.fh.advance(LONG_ABSENCE).await;

    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    routable_once_ready(&mut p, addr).await;
    p.fh.advance(STANDS_DOWN_WITHIN).await;
    assert_eq!(p.w_b2.metrics().repl_stand_down_total(), 1, "the copy stood down");
    assert!(p.fh.now_ms() < deadline, "before the ring deadline");

    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The worker returns while the backup's one connect still waits: the first
/// frame of its new generation confirms the verdict, which named the process
/// that died; the copy takes the call over and stands down on the reclaim.
#[tokio::test(start_paused = true)]
async fn returns_during_the_confirmation_and_the_copy_stands_down() {
    let mut p = place("not-ready-flip-returns-during", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.black_hole("b1");
    p.fh.mark("b1", None, "crash", "the process dies, its address answers no connect");
    p.w_b1.crash();
    p.fh.advance(FLIP_AFTER_THE_CRASH).await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(INSIDE_THE_CONNECT_BOUND).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated, "it still waits");

    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    p.fh.heal("b1", "b2");
    routable_once_ready(&mut p, addr).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "another generation confirms it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 1, "the copy took the call over");
    assert_eq!(metrics.repl_stand_down_total(), 1, "and stood down on the reclaim");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy serves nothing now");

    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The worker is back and streaming before the proxy reads the flip: the
/// verdict meets frames of the generation the flow already hears, so it is
/// refuted, and the worker's own reclaim takes its call back.
#[tokio::test(start_paused = true)]
async fn returns_before_the_flip_and_nothing_is_taken_over() {
    let mut p = place("not-ready-flip-returns-before", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let generation = p.w_b2.backup_flow_watermark("b1").gen;
    p.fh.mark("b1", None, "crash", "the process dies, its node lives");
    p.w_b1.crash();
    let addr = restarts_in_place(&mut p).await;
    p.proxy.set_address("b1", addr);
    let streaming =
        p.fh.pump_until(Duration::from_millis(50), Duration::from_secs(10), async || {
            p.w_b2.backup_flow_watermark("b1").gen != generation
        })
        .await;
    assert!(streaming, "the backup's flow hears the new generation");

    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "the flip is told");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 1, "the stream refutes it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");

    routable_once_ready(&mut p, addr).await;
    assert!(p.w_b1.serves(&p.ringing_ref), "the returned worker serves its call");
    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The worker is back and has reclaimed its call while the backup's puller
/// still backs off, and only then does the proxy read the flip. The verdict's
/// connect reads writes of the new generation, the reclaim write among them:
/// a copy made now would never meet it, so the verdict is refuted, and the
/// worker alone serves the call.
#[tokio::test(start_paused = true)]
async fn returns_and_reclaims_inside_the_backoff_before_the_flip() {
    let mut p = place("not-ready-flip-reclaimed-in-backoff", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.mark("b1", None, "crash", "the process dies, its node lives");
    p.w_b1.crash();
    p.fh.advance(INSIDE_THE_BACKOFF).await;
    let addr = restarts_in_place(&mut p).await;
    p.proxy.set_address("b1", addr);
    let reclaimed =
        p.fh.pump_until(Duration::from_millis(1), Duration::from_millis(500), async || {
            p.w_b1.serves(&p.ringing_ref) || p.w_b1.parks(&p.ringing_ref)
        })
        .await;
    assert!(reclaimed, "the returned worker reclaimed its call");

    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "the flip is told");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 1, "the worker's writes refute it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");

    routable_once_ready(&mut p, addr).await;
    assert!(!p.w_b2.serves(&p.ringing_ref), "no copy serves beside the worker");
    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The worker returns inside the copy's asked-again decision. The copy, written
/// over by the reclaim, applies no answer and stands down; the worker asks too
/// and alone dials the callee (ADR-0047 D4, D7).
#[tokio::test(start_paused = true)]
async fn an_undecided_call_whose_worker_returns_inside_the_decision_is_dialled_once() {
    let name = "not-ready-flip-undecided-returns";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "the process dies, its node lives");
    primary.crash();
    fh.advance(FLIP_AFTER_THE_CRASH).await;
    fh.flap_not_ready(&pri_ord);
    fh.advance(RETURNS_INSIDE_THE_DECISION).await;
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the copy asks the decision again");

    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let addr = primary.reboot().await;
    fh.note_worker_rebound(&pri_ord, addr);
    fh.flap_not_ready(&pri_ord);
    let ready = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            w_b1.is_ready() && w_b2.is_ready()
        })
        .await;
    assert!(ready, "the restarted worker became ready");
    fh.flap_ready(&pri_ord);
    proxy.set_address(&pri_ord, addr);
    proxy.set_health(&pri_ord, WorkerHealth::Alive);

    answer_and_hang_up(&mut call, &bob).await;
    fh.advance(Duration::from_secs(10)).await;
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    assert_eq!(dialled(&fh, &bob), 1, "the callee is dialled once");
    assert_eq!(consults.load(Ordering::SeqCst), 3, "asked by the copy and the restarted worker");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.metrics().repl_stand_down_total(), 1, "the copy stood down");
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// A live worker whose readiness flips with its stream open: the verdict is
/// told and refuted, the backup takes nothing over, and the worker reads ready
/// again and completes the call.
#[tokio::test(start_paused = true)]
async fn a_live_workers_flip_takes_nothing_over() {
    let mut p = place("not-ready-flip-live-worker", Callee::Ringing).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "the flip is told");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 1, "its stream refutes it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the ringing call stays with its primary");
    assert!(p.w_b1.serves(&p.ringing_ref), "the primary still serves it");

    p.fh.flap_ready("b1");
    p.proxy.set_health("b1", WorkerHealth::Alive);
    let answered = answer(&mut p).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// A refuted verdict is spent (ADR-0047, residual): the worker that dies while
/// it still reads not ready draws no second verdict, its ringing call waits for
/// a message or the departure, and the departure takes it over.
#[tokio::test(start_paused = true)]
async fn a_death_after_a_refuted_flip_waits_for_the_departure() {
    let mut p =
        place_ringing_for("not-ready-flip-refuted-then-dies", Callee::Ringing, LONG_RING).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_unconfirmed_total(), 1, "the stream refutes it");

    p.fh.mark("b1", None, "crash", "the process dies while it reads not ready");
    p.w_b1.crash();
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "no second verdict");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the call waits");

    p.fh.depart("b1");
    p.fh.advance(TAKEOVER_BOUND).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "the departure takes the call over");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// A second death right after the return, before the proxy read the worker
/// alive: no flip reads `Alive`, so no verdict; the copy stood down on the
/// reclaim, and the call waits for a message or the departure.
#[tokio::test(start_paused = true)]
async fn a_second_death_before_the_proxy_read_the_worker_alive_draws_no_verdict() {
    let mut p =
        place_ringing_for("not-ready-flip-loop-before-alive", Callee::Ringing, LONG_RING).await;
    let deadline = replicated_deadline(&p).await;
    dies_on_a_live_node(&mut p, Flip::AfterTheCrash).await;
    let _ = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    let stood_down =
        p.fh.pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            !p.w_b2.serves(&p.ringing_ref)
        })
        .await;
    assert!(stood_down, "the copy stands down on the reclaim");

    p.fh.mark("b1", None, "crash", "the process dies again before it is routable");
    p.w_b1.crash();
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "no second verdict");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the call waits");

    p.fh.depart("b1");
    p.fh.advance(TAKEOVER_BOUND).await;
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated + 1, "the departure's");
    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// A second death after the proxy read the worker alive: the second flip is a
/// second verdict, and the backup takes the call over again.
#[tokio::test(start_paused = true)]
async fn a_second_death_after_the_proxy_read_the_worker_alive_is_a_second_verdict() {
    let mut p =
        place_ringing_for("not-ready-flip-loop-after-alive", Callee::Ringing, LONG_RING).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    dies_on_a_live_node(&mut p, Flip::AfterTheCrash).await;
    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    routable_once_ready(&mut p, addr).await;
    p.fh.advance(STANDS_DOWN_WITHIN).await;
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy stood down");

    dies_on_a_live_node(&mut p, Flip::AfterTheCrash).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 2, "each flip of a worker read alive");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 2, "taken over each time");
    assert!(p.w_b2.serves(&p.ringing_ref), "the backup serves the call again");

    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// A planned drain's endpoint reads not ready and terminating: no verdict,
/// nothing taken over; the takeover waits for the endpoint to leave the slice.
#[tokio::test(start_paused = true)]
async fn a_terminating_flip_draws_no_verdict() {
    let mut p = place("not-ready-flip-terminating", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.withdraw_routing("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 0, "a planned drain is no verdict");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");

    p.fh.mark("b1", None, "crash", "the drained process exits");
    p.w_b1.crash();
    p.fh.depart("b1");
    p.fh.advance(TAKEOVER_BOUND).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "the departure takes the call over");
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 0, "and leaving is no verdict");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// A proxy with no cluster key tells nothing: the flip takes nothing over, and
/// the call waits for a message or the departure.
#[tokio::test(start_paused = true)]
async fn a_proxy_with_no_cluster_key_tells_no_flip() {
    let mut p = place("not-ready-flip-no-key", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    p.proxy.tell_no_dead_verdict();
    dies_on_a_live_node(&mut p, Flip::AfterTheCrash).await;
    p.fh.advance(CONFIRMED_BY_NOW).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 0, "nothing is told");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the call waits");

    p.fh.depart("b1");
    p.fh.advance(TAKEOVER_BOUND).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "the departure takes the call over");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// When a second proxy reads the same flip.
#[derive(Clone, Copy, Debug)]
enum SecondFlip {
    /// At the first's instant: its copy is the same verdict.
    SameInstant,
    /// While the first verdict's connect still waits: a later verdict, which
    /// joins the one wait.
    DuringTheConfirmation,
    /// Once the first verdict acted: a later verdict, which sweeps again.
    AfterTheTakeover,
}

/// The front proxy `place` made and the other one both read the flip of a dead
/// worker. However the second falls, the ringing call is taken over once and
/// ended once.
async fn two_proxies_read_the_flip(name: &str, second: SecondFlip) {
    let mut p = place(name, Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let workers = [("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())];
    let other: ProxySut = p.fh.spawn_proxy(SECOND_PROXY, &workers).await;

    if matches!(second, SecondFlip::DuringTheConfirmation) {
        p.fh.black_hole("b1");
    }
    p.fh.mark("b1", None, "crash", "the process dies, its node lives");
    p.w_b1.crash();
    p.fh.advance(FLIP_AFTER_THE_CRASH).await;
    // The harness's membership verbs drive the proxy spawned last.
    p.fh.flap_not_ready("b1");
    let verdicts = match second {
        SecondFlip::SameInstant => {
            p.proxy.set_endpoint_conditions("b1", false, false);
            1
        }
        SecondFlip::DuringTheConfirmation | SecondFlip::AfterTheTakeover => {
            p.fh.advance(Duration::from_millis(300)).await;
            p.proxy.set_endpoint_conditions("b1", false, false);
            2
        }
    };
    p.fh.advance(Duration::from_secs(3)).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), verdicts, "the workers' book of the verdicts");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "each is confirmed");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 1, "the call is taken over once");
    assert!(p.w_b2.serves(&p.ringing_ref), "the backup serves the ringing call");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    p.fh.heal("b1", "b2");
    drop(other);
    hang_up_and_depart(p).await;
}

#[tokio::test(start_paused = true)]
async fn a_second_proxys_flip_at_the_same_instant_is_the_same_verdict() {
    two_proxies_read_the_flip("not-ready-flip-two-proxies-same", SecondFlip::SameInstant).await;
}

#[tokio::test(start_paused = true)]
async fn a_second_proxys_flip_during_the_confirmation_joins_its_wait() {
    two_proxies_read_the_flip(
        "not-ready-flip-two-proxies-during",
        SecondFlip::DuringTheConfirmation,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_second_proxys_later_flip_takes_nothing_more_over() {
    two_proxies_read_the_flip("not-ready-flip-two-proxies-later", SecondFlip::AfterTheTakeover)
        .await;
}
