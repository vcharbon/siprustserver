//! **No worker reads ready while the workers answer.** An orchestrator that loses
//! the heartbeats of the workers' nodes marks every endpoint not ready, not
//! terminating, while the processes run and replicate. The proxy keeps probing
//! every worker present in the slice, and a view where none reads ready routes on
//! the probe's answers instead (ADR-0031 D1, ADR-0050 §11).
//!
//! ```text
//!   alice ── proxy ──┬── b1 not ready, answers its probe
//!                    └── b2 not ready, answers its probe
//!   new calls and in-dialog requests keep routing; a worker that stops
//!   answering is read dead and its backup takes its calls over
//! ```
//!
//! The axes: the flips' spacing and order ([`Flips`]), against the probe's round;
//! a worker's death before, inside and after the view; the view's end, by one
//! worker reading ready or by all; a worker restarted inside the view, its calls
//! parked; a proxy started inside it; a terminating worker.
//!
//! Beyond the failover sweep: the sweep holds every endpoint ready.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{worker_ordinals, FailoverHarness, ProxySut, WorkerHealth};
use scenario_harness::agent::Dialog;
use sip_proxy::health::HealthProbeConfig;

use crate::departure_support::{call_id_placed_on, spawn_workers_deciding, B1, B2, PROXY};
use crate::inflight_resend::settle_to_one_cdr;
use crate::not_ready_flip_tells_a_dead_verdict::{
    ends_at_the_deadline_on_the_copy, hang_up_and_depart, restarts_in_place,
};
use crate::ringing_call_taken_over_on_departure::{
    place_on, replicated_deadline, ring_deadline_decision, Callee, Cluster, Placed, ANSWER, OFFER,
};
use crate::swept_call_primary_returns::{answer, hang_up_and_settle};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROBE: &str = "127.0.0.1:5099";
/// A second proxy, started while no worker reads ready.
const SECOND_PROXY: &str = "127.0.0.1:5081";
const SECOND_PROBE: &str = "127.0.0.1:5098";
/// A ring deadline that outlasts every cell's view.
const LONG_RING: Duration = Duration::from_secs(60);
/// Past one probe round and its answer at the production cadence.
const ONE_ROUND: Duration = Duration::from_millis(1_100);
/// Past every confirmation of a live stream: its idle bound and one connect.
const CONFIRMED_BY_NOW: Duration = Duration::from_secs(5);

fn workers() -> [(&'static str, SocketAddr); 2] {
    [("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]
}

/// The latest verdict of the production probe.
fn read_dead_within() -> Duration {
    HealthProbeConfig::default().verdict_within()
}

/// The proxy with its health probe at the production cadence, over both workers.
async fn probing_proxy(fh: &FailoverHarness) -> ProxySut {
    fh.spawn_proxy_with_probe_cadence(PROXY, PROBE, &workers(), HealthProbeConfig::default()).await
}

/// Both workers behind a probing proxy, read alive by it.
async fn cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = probing_proxy(&fh).await;
    let decision = ring_deadline_decision(LONG_RING);
    let (w_b1, w_b2) = spawn_workers_deciding(&mut fh, decision).await;
    fh.advance(ONE_ROUND).await;
    assert_eq!(proxy.health("b1"), Some(WorkerHealth::Alive));
    assert_eq!(proxy.health("b2"), Some(WorkerHealth::Alive));
    Cluster { fh, alice, bob, proxy, w_b1, w_b2 }
}

/// One established and one ringing call on `b1`, both replicated to `b2`.
async fn placed(name: &str) -> Placed {
    place_on(cluster(name).await, Callee::Ringing, LONG_RING, Some(OFFER)).await
}

/// In which order the orchestrator flips the endpoints, and how far apart.
#[derive(Clone, Copy, Debug)]
struct Flips {
    first: &'static str,
    second: &'static str,
    gap: Duration,
}

impl Flips {
    async fn apply(self, p: &mut Placed) {
        p.fh.flap_not_ready(self.first);
        p.fh.advance(self.gap).await;
        p.fh.flap_not_ready(self.second);
    }
}

/// A new call the proxy placed and its caller ended: its primary and `call_ref`.
struct Completed {
    primary: String,
    call_ref: String,
}

/// A new call through the proxy answered and ended by its caller.
async fn a_new_call_completes(p: &mut Placed, call_id: String) -> Completed {
    let at = p.proxy.addr();
    a_new_call_through(p, at, call_id).await
}

/// [`a_new_call_completes`], its INVITE through the proxy at `at`.
async fn a_new_call_through(p: &mut Placed, at: SocketAddr, call_id: String) -> Completed {
    let tag = call_id.replace('@', "-");
    let mut call =
        p.alice.invite(&p.bob).with_sdp(OFFER).identity(call_id, tag).through(at).send().await;
    let mut uas = p.bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    let (primary, backup) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog: Dialog = call.ack().await;
    p.bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    p.fh.advance(Duration::from_millis(100)).await;
    let holder = if backup == "b1" { &p.w_b1 } else { &p.w_b2 };
    let call_ref = holder
        .scan_backed_up(&primary)
        .into_iter()
        .find(|r| *r != p.ringing_ref && *r != p.quiet_ref)
        .expect("the new call's replica on its backup");
    let mut bye = dialog.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    p.fh.advance(Duration::from_millis(100)).await;
    Completed { primary, call_ref }
}

/// Both endpoints flip, however spaced: new calls keep being placed, the calls
/// in place stay with their primary, and nothing is taken over. The first flip
/// is told, and refuted: `told` is whether the other worker backs a call of it.
async fn the_view_keeps_routing(name: &str, flips: Flips, told: u64) {
    let mut p = placed(name).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    flips.apply(&mut p).await;
    p.fh.advance(CONFIRMED_BY_NOW).await;

    assert!(p.proxy.placement_of("a-new-call").is_some(), "a worker takes a new call");
    let on_b1 = a_new_call_completes(&mut p, call_id_placed_on("b1", "inside-the-view")).await;
    assert_eq!(on_b1.primary, "b1", "placed as with both workers ready");
    let on_b2 = a_new_call_completes(&mut p, call_id_placed_on("b2", "inside-the-view")).await;
    assert_eq!(on_b2.primary, "b2", "placed as with both workers ready");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated, "nothing taken over");
    assert!(p.w_b1.serves(&p.ringing_ref), "the ringing call stays with its primary");
    let taken =
        p.w_b1.metrics().dead_verdicts_taken_total() + p.w_b2.metrics().dead_verdicts_taken_total();
    assert_eq!(taken, told, "the first flip is told, the one that empties the view is not");
    let refuted = p.w_b1.metrics().dead_verdicts_unconfirmed_total()
        + p.w_b2.metrics().dead_verdicts_unconfirmed_total();
    assert_eq!(refuted, told, "and its stream refutes it");

    let answered = answer(&mut p).await;
    p.fh.flap_ready("b1");
    p.fh.flap_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    assert_eq!(p.proxy.health("b1"), Some(WorkerHealth::Alive), "ready again");
    assert_eq!(p.proxy.health("b2"), Some(WorkerHealth::Alive), "ready again");
    hang_up_and_settle(p, answered, &[on_b1.call_ref, on_b2.call_ref]).await;
}

#[tokio::test(start_paused = true)]
async fn both_flips_at_one_instant_keep_routing() {
    let flips = Flips { first: "b1", second: "b2", gap: Duration::ZERO };
    the_view_keeps_routing("no-ready-same-instant", flips, 1).await;
}

#[tokio::test(start_paused = true)]
async fn the_second_flip_inside_a_probe_round_keeps_routing() {
    let flips = Flips { first: "b1", second: "b2", gap: Duration::from_millis(300) };
    the_view_keeps_routing("no-ready-inside-a-round", flips, 1).await;
}

#[tokio::test(start_paused = true)]
async fn the_backup_flipping_first_keeps_routing() {
    let flips = Flips { first: "b2", second: "b1", gap: Duration::from_millis(300) };
    the_view_keeps_routing("no-ready-backup-first", flips, 0).await;
}

#[tokio::test(start_paused = true)]
async fn the_primary_flipping_long_before_the_backup_is_routed_again_at_once() {
    let mut p = placed("no-ready-long-apart").await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    assert_eq!(p.proxy.placement_of("a-new-call").map(|(pri, _)| pri), Some("b2".to_string()));

    p.fh.flap_not_ready("b2");
    assert_eq!(
        p.proxy.health("b1"),
        Some(WorkerHealth::Alive),
        "at the flip that empties the view"
    );
    assert_eq!(
        p.proxy.health("b2"),
        Some(WorkerHealth::Alive),
        "at the flip that empties the view"
    );
    let new_call = a_new_call_completes(&mut p, call_id_placed_on("b1", "after-the-gap")).await;
    assert_eq!(new_call.primary, "b1", "the worker left first takes new calls again");

    let answered = answer(&mut p).await;
    p.fh.flap_ready("b1");
    p.fh.flap_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    hang_up_and_settle(p, answered, &[new_call.call_ref]).await;
}

/// The worker that flips first dies while the view reads no worker ready: its
/// verdict was refuted, and the probe's reading of its silence is a second one,
/// which its stream confirms.
#[tokio::test(start_paused = true)]
async fn a_worker_dying_inside_the_view_is_read_dead_and_taken_over() {
    let mut p = placed("no-ready-then-dies").await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let flips = Flips { first: "b1", second: "b2", gap: Duration::from_millis(300) };
    flips.apply(&mut p).await;
    p.fh.advance(CONFIRMED_BY_NOW).await;
    assert_eq!(
        p.w_b2.metrics().dead_verdicts_unconfirmed_total(),
        1,
        "the stream refutes the flip"
    );

    p.fh.mark("b1", None, "crash", "the process dies inside the view");
    p.w_b1.crash();
    p.fh.advance(read_dead_within() + CONFIRMED_BY_NOW).await;
    assert_eq!(p.proxy.health("b1"), Some(WorkerHealth::Dead), "a not-ready worker that is silent");
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 2, "the flip and the probe's reading");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated + 1, "the ringing call");
    assert!(p.w_b2.serves(&p.ringing_ref), "its backup serves the ringing call");
    assert_eq!(
        p.proxy.placement_of("a-new-call").map(|(pri, _)| pri),
        Some("b2".to_string()),
        "new calls go to the worker that answers"
    );

    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    p.fh.flap_ready("b2");
    hang_up_and_depart(p).await;
}

/// Every worker is silent and reads not ready: no worker takes a new call, the
/// caller gets the proxy's refusal, and none of the flips counts as an answer.
#[tokio::test(start_paused = true)]
async fn workers_that_read_not_ready_and_stay_silent_are_no_worker() {
    let mut c = cluster("no-ready-all-silent").await;
    c.fh.mark("b1", None, "crash", "every process dies");
    c.w_b1.crash();
    c.w_b2.crash();
    c.fh.flap_not_ready("b1");
    c.fh.flap_not_ready("b2");
    c.fh.advance(read_dead_within() + ONE_ROUND).await;
    assert!(c.proxy.placement_of("a-new-call").is_none(), "no worker takes a new call");
    let at = c.proxy.addr();
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(at).send().await;
    call.expect(503).await;
    drop(call);
    c.fh.advance(Duration::from_secs(1)).await;
}

/// One not-ready worker among ready ones leaves the routing, draws its verdict
/// and stays out however well it answers.
#[tokio::test(start_paused = true)]
async fn one_not_ready_among_ready_ones_stays_out_however_it_answers() {
    let mut p = placed("no-ready-one-among-ready").await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "the flip is told");
    assert_eq!(p.w_b2.metrics().dead_verdicts_unconfirmed_total(), 1, "its stream refutes it");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated, "nothing taken over");
    assert_eq!(p.proxy.health("b1"), None, "out of the routing");
    assert_eq!(p.proxy.placement_of("a-new-call").map(|(pri, _)| pri), Some("b2".to_string()));

    p.fh.flap_ready("b1");
    p.fh.advance(ONE_ROUND).await;
    assert_eq!(p.proxy.health("b1"), Some(WorkerHealth::Alive), "routed again");
    let answered = answer(&mut p).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// A not-ready worker among ready ones dies after its flip was refuted: the
/// proxy still probes it, reads its silence and tells a second verdict.
#[tokio::test(start_paused = true)]
async fn a_death_after_a_refuted_flip_is_read_by_the_probe() {
    let mut p = placed("no-ready-refuted-then-dies").await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    assert_eq!(
        p.w_b2.metrics().dead_verdicts_unconfirmed_total(),
        1,
        "its stream refutes the flip"
    );

    p.fh.mark("b1", None, "crash", "the process dies while it reads not ready");
    p.w_b1.crash();
    p.fh.advance(read_dead_within() + CONFIRMED_BY_NOW).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 2, "the probe's reading is told");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated + 1, "the ringing call");
    assert!(p.w_b2.serves(&p.ringing_ref), "its backup serves the ringing call");

    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// A crash loop with a ready peer: the restarted process answers its probe while
/// its endpoint still reads not ready, reclaims, and dies again. Its copies stood
/// down, and the probe's reading of its silence is the second verdict, its endpoint
/// never having read ready in between.
#[tokio::test(start_paused = true)]
async fn a_process_that_answered_while_not_ready_and_dies_again_is_a_second_verdict() {
    let mut p = placed("no-ready-crash-loop").await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.mark("b1", None, "crash", "the process dies, its node lives");
    p.w_b1.crash();
    p.fh.advance(Duration::from_millis(1_500)).await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(Duration::from_millis(500)).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "its backup took the call over");

    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    p.proxy.set_address("b1", addr);
    let answering =
        p.fh.pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            p.w_b1.is_ready()
                && p.proxy.probed_health("b1") == Some(WorkerHealth::Alive)
                && !p.w_b2.serves(&p.ringing_ref)
        })
        .await;
    assert!(answering, "it answers its probe and its reclaim stood the copy down");
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "the first death's");
    // Past every probe that left before the flip: only a later round reads it.
    p.fh.advance(read_dead_within()).await;

    p.fh.mark("b1", None, "crash", "the process dies again, never ready in between");
    p.w_b1.crash();
    p.fh.advance(read_dead_within() + CONFIRMED_BY_NOW).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 2, "the probe's reading is told");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated + 2, "taken over again");
    assert!(p.w_b2.serves(&p.ringing_ref), "its backup serves the ringing call");

    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// A crash on a live node with a ready peer: the flip is the verdict, and the
/// probe reading the same silence tells nothing more.
#[tokio::test(start_paused = true)]
async fn the_probe_reading_a_flipped_crash_adds_no_second_verdict() {
    let mut p = placed("no-ready-crash-one-verdict").await;
    let deadline = replicated_deadline(&p).await;
    p.fh.mark("b1", None, "crash", "the process dies, its node lives");
    p.w_b1.crash();
    p.fh.advance(Duration::from_millis(1_500)).await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(read_dead_within() + ONE_ROUND).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "one verdict per death");
    assert!(p.w_b2.serves(&p.ringing_ref), "its backup serves the ringing call");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// One worker reads ready again: routing follows readiness at once. The worker
/// still not ready leaves the routing with no verdict, since nothing about it
/// changed, and returns once it reads ready.
#[tokio::test(start_paused = true)]
async fn one_worker_ready_again_ends_the_view_with_no_verdict() {
    let mut p = placed("no-ready-one-returns").await;
    let flips = Flips { first: "b1", second: "b2", gap: Duration::ZERO };
    flips.apply(&mut p).await;
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let told = p.w_b1.metrics().dead_verdicts_taken_total();

    p.fh.flap_ready("b1");
    p.fh.advance(ONE_ROUND).await;
    assert_eq!(p.proxy.health("b1"), Some(WorkerHealth::Alive), "stays routed");
    assert_eq!(p.proxy.health("b2"), None, "the other leaves the routing");
    assert_eq!(p.proxy.placement_of("a-new-call").map(|(pri, _)| pri), Some("b1".to_string()));
    assert_eq!(p.w_b1.metrics().dead_verdicts_taken_total(), told, "no verdict on the other");

    p.fh.flap_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    assert_eq!(p.proxy.health("b2"), Some(WorkerHealth::Alive), "back in the routing");
    let answered = answer(&mut p).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// The view ends on the backup's return while the primary still reads not
/// ready, as any single not-ready worker, and the primary's flip back reopens it.
#[tokio::test(start_paused = true)]
async fn a_flapping_view_routes_on_each_reading() {
    let mut p = placed("no-ready-flapping").await;
    let flips = Flips { first: "b1", second: "b2", gap: Duration::ZERO };
    flips.apply(&mut p).await;
    p.fh.advance(ONE_ROUND).await;
    p.fh.flap_ready("b2");
    assert_eq!(p.proxy.health("b1"), None, "b1 left the routing");
    p.fh.flap_not_ready("b2");
    assert_eq!(p.proxy.health("b1"), Some(WorkerHealth::Alive), "b1 answered all along");
    p.fh.advance(ONE_ROUND).await;
    assert_eq!(p.proxy.health("b2"), Some(WorkerHealth::Alive));
    p.fh.flap_ready("b1");
    p.fh.flap_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    let answered = answer(&mut p).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// A terminating worker never counts: the other one, not ready and answering,
/// takes every new call.
#[tokio::test(start_paused = true)]
async fn a_terminating_worker_is_never_routed_on_its_answers() {
    let mut p = placed("no-ready-terminating").await;
    let deadline = replicated_deadline(&p).await;
    p.fh.withdraw_routing("b1");
    p.fh.flap_not_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    assert_eq!(p.proxy.health("b1"), None, "out of the routing");
    assert_eq!(p.proxy.health("b2"), Some(WorkerHealth::Alive), "routed on its answers");
    assert_eq!(p.proxy.placement_of("a-new-call").map(|(pri, _)| pri), Some("b2".to_string()));

    p.fh.mark("b1", None, "crash", "the drained process exits");
    p.w_b1.crash();
    p.fh.depart("b1");
    p.fh.advance(Duration::from_millis(500)).await;
    assert!(p.w_b2.serves(&p.ringing_ref), "the departure takes the call over");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    p.fh.flap_ready("b2");
    hang_up_and_depart(p).await;
}

/// A worker restarted inside the view holds its reclaimed calls parked until its
/// own endpoint reads ready. Its answers route new calls to it meanwhile: they
/// complete beside the parked calls, which stay parked, a message for one arming
/// it, and the endpoint's return arming the rest.
#[tokio::test(start_paused = true)]
async fn a_worker_restarted_inside_the_view_takes_new_calls_beside_its_parked_ones() {
    let mut p = placed("no-ready-restart-inside").await;
    let flips = Flips { first: "b1", second: "b2", gap: Duration::ZERO };
    flips.apply(&mut p).await;
    p.fh.advance(ONE_ROUND).await;

    p.w_b1.crash();
    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    p.proxy.set_address("b1", addr);
    let answering =
        p.fh.pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            p.w_b1.is_ready() && p.proxy.health("b1") == Some(WorkerHealth::Alive)
        })
        .await;
    assert!(answering, "the restarted worker answers its probe");
    assert!(p.w_b1.parks(&p.ringing_ref), "its reclaimed call waits for its endpoint");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy stood down or never was");

    let new_call = a_new_call_completes(&mut p, call_id_placed_on("b1", "beside-parked")).await;
    assert_eq!(new_call.primary, "b1", "placed on the worker that answers");
    assert!(p.w_b1.parks(&p.ringing_ref), "the new call arms nothing");

    p.fh.flap_ready("b1");
    p.fh.flap_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    assert!(p.w_b1.serves(&p.ringing_ref), "armed once its endpoint reads ready");
    let answered = answer(&mut p).await;
    hang_up_and_settle(p, answered, &[new_call.call_ref]).await;
}

/// A proxy started inside the view holds no memory of an earlier one: its first
/// round reads the workers alive, and it routes.
#[tokio::test(start_paused = true)]
async fn a_proxy_started_inside_the_view_routes_on_its_first_round() {
    let mut p = placed("no-ready-proxy-started-inside").await;
    let answered = answer(&mut p).await;
    let flips = Flips { first: "b1", second: "b2", gap: Duration::ZERO };
    flips.apply(&mut p).await;
    p.fh.advance(ONE_ROUND).await;

    let started =
        p.fh.spawn_proxy_with_probe_cadence(
            SECOND_PROXY,
            SECOND_PROBE,
            &workers(),
            HealthProbeConfig::default(),
        )
        .await;
    for ordinal in ["b1", "b2"] {
        started.set_health(ordinal, WorkerHealth::Unknown);
        started.set_endpoint_conditions(ordinal, false, false);
    }
    assert!(started.placement_of("a-new-call").is_none(), "no round read a worker yet");
    p.fh.advance(ONE_ROUND).await;
    let call_id = call_id_placed_on("b1", "started-inside");
    let new_call = a_new_call_through(&mut p, started.addr(), call_id).await;
    assert_eq!(new_call.primary, "b1");

    // The harness's membership verbs drive the proxy spawned last.
    p.fh.flap_ready("b1");
    p.fh.flap_ready("b2");
    p.proxy.set_endpoint_conditions("b1", true, false);
    p.proxy.set_endpoint_conditions("b2", true, false);
    p.fh.advance(ONE_ROUND).await;
    drop(started);
    hang_up_and_settle(p, answered, &[new_call.call_ref]).await;
}

/// A dialog ended across the gap between the flips: the caller's BYE reaches the
/// backup while the primary alone reads not ready; the call settles to one CDR
/// and the other call goes on once the view routes both.
#[tokio::test(start_paused = true)]
async fn a_bye_in_the_gap_between_the_flips_ends_its_call() {
    let mut p = placed("no-ready-bye-in-the-gap").await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(CONFIRMED_BY_NOW).await;
    let mut bye = p.quiet.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    p.fh.advance(ONE_ROUND).await;

    p.fh.flap_not_ready("b2");
    p.fh.advance(ONE_ROUND).await;
    let mut answered = answer(&mut p).await;
    let mut bye = answered.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    p.fh.flap_ready("b1");
    p.fh.flap_ready("b2");
    p.fh.advance(Duration::from_secs(60)).await;
    for call_ref in [&p.ringing_ref, &p.quiet_ref] {
        settle_to_one_cdr(&p.fh, &[&p.alice, &p.bob], &[&p.w_b1, &p.w_b2], call_ref).await;
    }
    drop(p.proxy);
}
