//! **Reclaim-hydration pin for the per-leg no-answer guard.**
//!
//! The endurance defect: a `kill_worker` + reboot reclaim restores a CONFIRMED
//! call whose replicated `call.timers` ledger still carries a live per-b-leg
//! `NoAnswer` entry — the entry's cancel died with the crashed node. Pre-fix the
//! restored entry re-fired and tore the established call down (~107 drops per
//! kill event). The guard must absorb the fire (the timer's own leg is
//! Confirmed), scrub the spent entry so a later reclaim cannot re-fire it, and
//! leave the call fully serviceable.
//!
//! Unlike the `b2bua-harness` sibling (`no_answer_absorb.rs`, which re-arms the
//! entry via a probe service on a live call), this test drives the REAL
//! hydration path: the stale entry is implanted into the surviving backup's
//! replica body, the primary reboots empty, bootstrap re-hydrates `pri:{self}`
//! from the peer and the bulk `router::reclaim_all` materialises the call +
//! re-arms its timers (`sanitize_restored_timers` + `TimerService::restore`).
//! The fire then reaches the `no-answer` rule exactly as it did on the cluster.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use call::{parse_call_ref, CallBodyCodec, CdrEventType, MsgpackCodec, TimerType};
use failover_harness::{
    worker_ordinals, FailoverHarness, PartitionRole, ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::Agent;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const BAK: PartitionRole = PartitionRole::Backup;

/// The route-supplied ring deadline: armed at route time, cancelled by the
/// answer — so the live call's ledger takes the production arm-then-cancel
/// shape before the surgery re-implants the stale entry.
const NO_ANSWER_SEC: i64 = 15;

/// How far after the crash the implanted stale entry fires: past the reboot +
/// ready + bulk-reclaim window, far below the 300 s keepalive interval.
const STALE_FIRE_AFTER_MS: i64 = 90_000;

/// Decision routing every call to bob, arming the per-b-leg `NoAnswer`.
fn no_answer_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The first replicated ref in `bak:{primary}` on the backup.
async fn find_backed_up_ref(backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary).await {
            if let Some(p) = parse_call_ref(&rf) {
                assert_eq!(p.primary, primary, "backed-up ref encodes its primary");
            }
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the backup");
}

/// Does the replica body the backup holds for `call_ref` carry a `NoAnswer`
/// ledger entry?
async fn replica_carries_no_answer(
    backup: &ReplicatedB2buaSut,
    primary: &str,
    call_ref: &str,
) -> bool {
    let Some(body) = backup.get(BAK, primary, call_ref).await else {
        return false;
    };
    let call = MsgpackCodec::new().decode(&body).expect("replica body decodes");
    call.timers.iter().any(|t| t.timer_type == TimerType::NoAnswer)
}

/// Answer any pending keepalive OPTIONS on either peer (the reclaim sweep
/// de-correlates the restored keepalive inside `[now, fire_at]`, so it may land
/// anywhere in the post-reclaim window).
async fn service_pending_options(alice: &Agent, bob: &Agent) {
    if let Some(mut t) = alice.try_receive_tolerating("OPTIONS", &[]).await {
        t.respond(200, "OK").await;
    }
    if let Some(mut t) = bob.try_receive_tolerating("OPTIONS", &[]).await {
        t.respond(200, "OK").await;
    }
}

/// A stale `NoAnswer` entry restored by the REAL reboot reclaim on a Confirmed
/// call is absorbed: no teardown, the spent entry is scrubbed out of the
/// replicated ledger (a second reclaim cannot re-fire it), and the call remains
/// fully serviceable end-to-end.
#[tokio::test(start_paused = true)]
async fn stale_no_answer_restored_by_reclaim_is_absorbed_and_scrubbed() {
    let mut fh = FailoverHarness::new("s11-stale-noanswer-reclaim", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane)); // lanes registered for reporting only

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            no_answer_decision(),
            Arc::new(NoopLimiter),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2",
            "b2",
            B2,
            &["b1"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            no_answer_decision(),
            Arc::new(NoopLimiter),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish alice ⇄ bob on the HRW primary (NoAnswer armed at route,
    //    cancelled by the answer — the production ledger shape) ───────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(b1.serves(&call_ref), "primary serves the established call");
    assert!(b2.is_synchronized_backup(&call_ref).await, "backup synchronized");
    assert!(
        !replica_carries_no_answer(b2, &primary_ord, &call_ref).await,
        "the answer cancelled the route-time NoAnswer in the replicated ledger",
    );

    // ── crash the primary; the call is quiescent during the outage ───────────
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_secs(1)).await;

    // ── SURGERY: the stale entry whose cancel died with the crashed node — the
    //    Confirmed replica body now carries a live NoAnswer for its b-leg ─────
    let fire_at = fh.now_ms() + STALE_FIRE_AFTER_MS;
    b2.implant_stale_no_answer(BAK, &primary_ord, &call_ref, fire_at).await;
    assert!(
        replica_carries_no_answer(b2, &primary_ord, &call_ref).await,
        "the replica body carries the implanted stale NoAnswer entry",
    );

    // ── reboot → bootstrap re-hydrates pri:{self} → REAL bulk reclaim ────────
    let b1_addr = b1.reboot().await; // NEW pod IP
    proxy.set_address(&primary_ord, b1_addr);
    fh.note_worker_rebound(&primary_ord, b1_addr);
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    b2.simulate_peer_added(&primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    fh.advance(Duration::from_secs(10)).await; // ReclaimAll (smoothed)
    assert!(b1.serves(&call_ref), "the rebooted primary reclaimed the Confirmed call");
    assert_eq!(b1.active_calls(), 1, "exactly the reclaimed call is live");

    // ── cross the stale NoAnswer deadline. Pre-fix this tore the reclaimed
    //    call down; the per-leg guard must absorb it (its b-leg is Confirmed).
    //    Advance in small steps, answering any de-correlated keepalive OPTIONS
    //    that lands in the window. ───────────────────────────────────────────
    while fh.now_ms() < fire_at + 1_000 {
        fh.advance(Duration::from_secs(1)).await;
        service_pending_options(&alice, &bob).await;
    }

    // ABSORBED: the call survives — nobody was CANCELled, BYE'd, or 480'd.
    assert!(b1.serves(&call_ref), "the Confirmed call survives the stale fire");
    assert_eq!(b1.active_calls(), 1, "no teardown side-effects");

    // SCRUBBED: the spent entry is cancelled out of the ledger and the refreshed
    // replica no longer carries it — the body a SECOND reclaim would hydrate
    // from cannot re-fire it.
    let scrubbed = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(30), async || {
            service_pending_options(&alice, &bob).await;
            !replica_carries_no_answer(b2, &primary_ord, &call_ref).await
        })
        .await;
    assert!(scrubbed, "the absorb's scrub reaches the replicated ledger");

    // ── the surviving call terminates normally: BYE end-to-end ───────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.advance(Duration::from_secs(1)).await;
    failover_harness::assert_call_fully_released(&[&*b1, &*b2], &call_ref).await;

    // No trace of the absorbed fire in the CDR stream: no timeout event, no
    // no-answer reason, on any incarnation of either node.
    for record in b1.cdr_records().iter().chain(b2.cdr_records().iter()) {
        assert!(
            !record.events.iter().any(|e| e.event_type == CdrEventType::Timeout),
            "no timeout CDR event for the absorbed fire",
        );
        assert!(
            !record
                .events
                .iter()
                .any(|e| e.reason.as_deref().is_some_and(|r| r.contains("no_answer"))),
            "no no-answer marker on the absorbed fire",
        );
    }

    drop((w_b1, w_b2, proxy));
}
