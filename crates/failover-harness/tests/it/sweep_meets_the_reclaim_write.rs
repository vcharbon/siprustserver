//! **A dead verdict's sweep that meets the restarted worker's reclaim write.**
//! The first frame of the new generation is a `Noop`, which confirms the verdict
//! (ADR-0047 D1), and the reclaim write follows it while the backup's sweep goes
//! over the worker's calls, one at a time. A copy made before the write stands
//! down on it. From the first write on the sweep makes none: a copy read from a
//! replica the write already reached meets no later write to stand down on, and
//! both nodes would act on the call.
//!
//! ```text
//!   b1's process dies, its address answers no connect
//!   b1 not ready → proxy tells b2 → b2's connect waits
//!   b1 restarts: b2 hears a Noop of the new generation → confirmed → sweep
//!   b1's reclaim write reaches b2 ... the sweep is over the first call, or the second
//! ```
//!
//! The sweep's duration is the axis: at once, across the write, just past it, far
//! past it.
//!
//! Beyond the failover sweep: the duration of a takeover sweep over a partition.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::{FailoverHarness, WorkerHealth};

use crate::departure_support::{bring_up_deciding, call_id_placed_on, CountedDecision};
use crate::inflight_resend::settle_to_one_cdr;
use crate::not_ready_flip_tells_a_dead_verdict::{
    ends_at_the_deadline_on_the_copy, ends_at_the_deadline_on_the_primary, hang_up_and_depart,
    restarts_in_place, routable_once_ready, DECISION_DELAY, FLIP_AFTER_THE_CRASH,
    INSIDE_THE_CONNECT_BOUND, STANDS_DOWN_WITHIN,
};
use crate::ringing_call_taken_over_on_departure::{
    place, replicated_deadline, Callee, ANSWER, OFFER,
};
use crate::swept_call_primary_returns::hang_up_quiet_and_settle;
use crate::undecided_call_settled_on_departure::{dialled, rejections_to};

const ALICE: &str = "127.0.0.1:5060";
const CAROL: &str = "127.0.0.1:5061";
const BOB: &str = "127.0.0.1:5070";

/// How long the backup's sweep spends on each call with work in flight. The sweep
/// starts at the new generation's first `Noop`, and the reclaim write lands 4 ms
/// after it.
const AT_ONCE: Duration = Duration::ZERO;
/// The write lands between the sweep's first call and its second.
const ACROSS_THE_WRITE: Duration = Duration::from_millis(3);
/// The write lands at the instant the sweep takes its first call: the copy is
/// read before the write and marked after it, so the write finds no mark.
const AT_THE_WRITE: Duration = Duration::from_millis(4);
const PAST_THE_WRITE: Duration = Duration::from_millis(30);
const FAR_PAST_THE_WRITE: Duration = Duration::from_millis(500);

/// Two ringing calls and a quiet one, which the sweep passes first, its refs being
/// sorted: the sweep makes `copies` of the ringing ones before the write and none
/// after, every copy stands down, and the worker alone ends each call, one final
/// and one CDR each.
async fn ringing_calls(name: &str, per_call: Duration, copies: u64) {
    let mut p = place(name, Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;

    let second_cid = call_id_placed_on("b1", "second");
    let invite = p.alice.invite(&p.bob).identity(second_cid, "second-tag");
    let mut second = invite.with_sdp(OFFER).through(p.proxy.addr()).send().await;
    second.expect(100).await;
    let mut second_at_bob = p.bob.receive("INVITE").await;
    second_at_bob.respond(180, "Ringing").await;
    second.expect(180).await;
    p.fh.advance(Duration::from_millis(500)).await;
    let refs = p.w_b2.scan_backed_up("b1");
    let second_ref = refs.iter().find(|r| r.contains("second")).expect("replicated").clone();

    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    p.fh.turn_points().pace_sweep(&[Duration::ZERO, per_call]);
    p.fh.black_hole("b1");
    p.fh.mark("b1", None, "crash", "the process dies, its address answers no connect");
    p.w_b1.crash();
    p.fh.advance(FLIP_AFTER_THE_CRASH).await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(INSIDE_THE_CONNECT_BOUND).await;

    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    p.fh.heal("b1", "b2");
    routable_once_ready(&mut p, addr).await;
    p.fh.advance(STANDS_DOWN_WITHIN).await;

    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "a Noop of the generation confirms");
    assert_eq!(metrics.repl_takeover_hydrated_total() - hydrated, copies, "the copies made");
    assert_eq!(metrics.repl_stand_down_total(), copies, "every copy made stood down");
    assert!(!p.w_b2.serves(&p.ringing_ref), "no copy serves beside the worker");
    assert!(!p.w_b2.serves(&second_ref), "no copy serves beside the worker");

    second_at_bob.respond(486, "Busy Here").await;
    p.bob.receive("ACK").await;
    second.expect(486).await;
    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[second_ref]).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_at_once_makes_the_copies_that_stand_down() {
    ringing_calls("sweep-ringing-at-once", AT_ONCE, 2).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_across_the_reclaim_write_stops_after_its_first_copy() {
    ringing_calls("sweep-ringing-across", ACROSS_THE_WRITE, 1).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_at_the_reclaim_write_stands_its_one_copy_down() {
    ringing_calls("sweep-ringing-at-the-write", AT_THE_WRITE, 1).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_past_the_reclaim_write_makes_no_copy_of_a_ringing_call() {
    ringing_calls("sweep-ringing-past", PAST_THE_WRITE, 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_far_past_the_reclaim_write_makes_no_copy_of_a_ringing_call() {
    ringing_calls("sweep-ringing-far", FAR_PAST_THE_WRITE, 0).await;
}

/// A restart that dies before it writes. The first incarnation wrote on the flow;
/// the second's `Noop` is the generation the verdict reads, and it dies with
/// nothing written, its own pull from the backup held so it reclaims nothing. The
/// stream's end confirms the verdict, and an older generation's write is no
/// return: the sweep takes the ringing call over and ends it at its deadline.
#[tokio::test(start_paused = true)]
async fn a_restart_that_dies_before_it_writes_leaves_its_calls_to_the_sweep() {
    let mut p = place("sweep-restart-dies-unwritten", Callee::Ringing).await;
    let deadline = replicated_deadline(&p).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let generation = p.w_b2.backup_flow_watermark("b1").gen;

    p.fh.black_hole("b2");
    p.fh.mark("b1", None, "crash", "the process dies, its node lives");
    p.w_b1.crash();
    let _ = restarts_in_place(&mut p).await;
    let streaming =
        p.fh.pump_until(Duration::from_millis(1), Duration::from_secs(10), async || {
            p.w_b2.backup_flow_watermark("b1").gen != generation
        })
        .await;
    assert!(streaming, "the backup's flow hears the new generation");

    p.fh.flap_not_ready("b1");
    p.fh.advance(Duration::from_millis(2)).await;
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");
    p.fh.mark("b1", None, "crash", "the restarted process dies with nothing written");
    p.w_b1.crash();
    p.fh.advance(STANDS_DOWN_WITHIN).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "the stream's end confirms it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 1, "the sweep takes it over");
    assert!(p.w_b2.serves(&p.ringing_ref), "the backup serves the ringing call");

    p.fh.heal("b1", "b2");
    ends_at_the_deadline_on_the_copy(&mut p, deadline).await;
    hang_up_and_depart(p).await;
}

/// Two undecided calls: however the sweep falls against the reclaim write, one
/// node acts on each decision, and each callee is dialled once (ADR-0047 D4, D7).
async fn undecided_calls(name: &str, per_call: Duration) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let carol = fh.agent("carol", CAROL).await;
    let bob = fh.agent("bob", BOB).await;
    let route = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;

    // A caller each: the worker answers them in no fixed order.
    let mut calls = Vec::new();
    for (caller, label) in [(&alice, "first"), (&carol, "second")] {
        let invite = caller.invite(&bob).identity(call_id_placed_on("b1", label), label);
        let mut call = invite.with_sdp(OFFER).through(proxy.addr()).send().await;
        call.expect(100).await;
        calls.push(call);
    }
    fh.advance(Duration::from_millis(100)).await;
    let refs = w_b2.scan_backed_up("b1");
    assert_eq!(refs.len(), 2, "both calls reached their backup at birth");

    fh.turn_points().pace_sweep(&[per_call]);
    fh.black_hole("b1");
    fh.mark("b1", None, "crash", "the process dies, its address answers no connect");
    w_b1.crash();
    fh.advance(FLIP_AFTER_THE_CRASH).await;
    fh.flap_not_ready("b1");
    fh.advance(INSIDE_THE_CONNECT_BOUND).await;

    let addr = w_b1.reboot().await;
    fh.note_worker_rebound("b1", addr);
    fh.flap_not_ready("b1");
    fh.heal("b1", "b2");
    let ready = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || w_b1.is_ready())
        .await;
    assert!(ready, "the restarted worker became ready");
    fh.flap_ready("b1");
    proxy.set_address("b1", addr);
    proxy.set_health("b1", WorkerHealth::Alive);

    let mut at_bob = [bob.receive("INVITE").await, bob.receive("INVITE").await];
    for uas in &mut at_bob {
        uas.respond(200, "OK").with_sdp(ANSWER).await;
    }
    let mut dialogs = Vec::new();
    for mut call in calls {
        call.expect(200).await;
        dialogs.push(call.ack().await);
        bob.receive("ACK").await;
    }
    for dialog in &mut dialogs {
        scenario_harness::callflow::hangup(dialog, &bob).await;
    }
    fh.advance(DECISION_DELAY + Duration::from_secs(1)).await;
    assert_eq!(dialled(&fh, &bob), 2, "each callee is dialled once");
    assert_eq!(rejections_to(&fh, &alice), 0, "the first caller is not refused");
    assert_eq!(rejections_to(&fh, &carol), 0, "the second caller is not refused");
    for call_ref in &refs {
        settle_to_one_cdr(&fh, &[&alice, &carol, &bob], &[&w_b1, &w_b2], call_ref).await;
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_sweep_at_once_dials_each_undecided_callee_once() {
    undecided_calls("sweep-undecided-at-once", AT_ONCE).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_across_the_reclaim_write_dials_each_undecided_callee_once() {
    undecided_calls("sweep-undecided-across", ACROSS_THE_WRITE).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_at_the_reclaim_write_dials_each_undecided_callee_once() {
    undecided_calls("sweep-undecided-at-the-write", AT_THE_WRITE).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_past_the_reclaim_write_dials_each_undecided_callee_once() {
    undecided_calls("sweep-undecided-past", PAST_THE_WRITE).await;
}

#[tokio::test(start_paused = true)]
async fn a_sweep_far_past_the_reclaim_write_dials_each_undecided_callee_once() {
    undecided_calls("sweep-undecided-far", FAR_PAST_THE_WRITE).await;
}
