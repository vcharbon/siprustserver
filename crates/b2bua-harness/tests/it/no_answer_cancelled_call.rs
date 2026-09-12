//! The `no-answer` timer on a call the CALLER already CANCELed
//!.
//!
//! Caller CANCELs the initial INVITE pre-18x and the callee answers NOTHING —
//! ever (a callee host gone dark mid-setup). The caller's CANCEL leaves the
//! b-leg at `state = Trying`, `disposition = Cancelling` for the whole
//! terminating window, so a state-only spent-check reads the leg as live.
//! Pre-fix the per-b-leg `NoAnswer` fire then ran the full failover treatment
//! on the abandoned call: a `/calls/failure` consult (origin
//! `no_answer_timeout`) and a second final (480) on the a-leg's initial-INVITE
//! server transaction — already finalized 487 and ACKed (RFC 3261 §17.2.1).
//!
//! The invariant: a leg/call already going away makes NO forward progress —
//! no failure consult, no new final on a transaction that already carries
//! one. `BeginTermination` scrubs the per-leg `NoAnswer` entries and every
//! service watchdog at the source; the executor absorbs a fire that still
//! reaches a going-away call unless the rule is a teardown rule; and the
//! `no-answer` spent-check (a teardown rule) absorbs the reclaim-restored
//! shape — pinned at the rule seam in `b2bua/tests/rules.rs`. The second test
//! pins the source scrub for a service watchdog; the third pins the
//! `handle-timeout` sibling: the b-leg INVITE transaction backstop firing on
//! a caller-CANCELed leg resolves it locally.
//!
//! The b-leg CANCEL toward the response-less callee is HELD by sip-txn for
//! the grace window, then sent regardless (RFC 3261 §9.1 bounded per
//! ADR-0028) — bob sees Timer-A INVITE retransmits plus exactly ONE
//! grace-expiry CANCEL, which he (gone dark) never answers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::CdrRecord;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallTreatment, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The route-supplied per-b-leg ring deadline.
const NO_ANSWER_SEC: i64 = 5;

/// A failover-capable scripted backend: every call routes to `port` with a
/// `NoAnswer` ring deadline and a `callback_context` (so the `no-answer` /
/// `handle-timeout` consult path is REACHABLE); every `/calls/failure`
/// consult increments `consults` and rejects 480 — loud on the wire if a
/// regression ever consults for an abandoned call.
fn failover_capable_decision(port: u16, consults: Arc<AtomicUsize>) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", port);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                r.callback_context = Some("nk068".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |_req| {
                consults.fetch_add(1, Ordering::SeqCst);
                CallTreatment::Reject(RejectDecision {
                    reject_code: 480,
                    reject_reason: Some("Temporarily Unavailable".into()),
                    update_headers: None,
                })
            })
            .build(),
    )
}

fn reasons_of(cdr: &CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

/// Probe service for the reclaim shape: re-arms the b-leg's `NoAnswer` ledger
/// entry AFTER the caller's CANCEL moved the call to Terminating — the entry a
/// reboot reclaim restores when its cancel died with the crashed node.
mod stalerestore {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;

    /// When the probe injects the stale entry (after the caller's CANCEL,
    /// inside the 32 s terminating window).
    pub const INJECT_AT_SEC: i64 = 8;
    /// The stale entry's own remaining delay after injection.
    pub const STALE_FIRE_SEC: i64 = 3;

    define_service! {
        id: "stalerestore",
        machine: STALERESTORE,
        states: SrState { Waiting },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(SrState::Waiting.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(STALERESTORE, "inject"),
                    delay: TimerDelay::secs(INJECT_AT_SEC),
                    leg_id: None,
                },
            ]))
        },
        rules: [ inject_stale_entry() ],
    }

    /// Would re-arm the b-leg's `NoAnswer` on the terminating call — the
    /// ledger shape a reboot reclaim restores when the entry's cancel died
    /// with the node. Its own watchdog is disarmed at the caller's CANCEL,
    /// so this never runs.
    fn inject_stale_entry() -> RuleDefinition {
        sm_rule! {
            id: "stalerestore-inject",
            machine: STALERESTORE,
            active: [ SrState::Waiting ],
            transitions: [ SrState::Waiting => Terminal ],
            effects: [
                Effect::GuardTimer {
                    timer: TimerType::NoAnswer,
                    label: "re-arm the stale NoAnswer entry",
                },
            ],
            matcher: Match::timer().timer_type(TimerType::service(STALERESTORE, "inject")),
            handle: |ctx: &RuleContext| {
                let b = ctx
                    .call
                    .b_legs()
                    .first()
                    .expect("the cancelled call still carries its b-leg")
                    .leg_id
                    .clone();
                Some(RuleHandleResult::new(vec![
                    RuleAction::ScheduleTimer {
                        timer_type: TimerType::NoAnswer,
                        delay: TimerDelay::secs(STALE_FIRE_SEC),
                        leg_id: Some(b),
                    },
                    RuleAction::ClearState { machine: STALERESTORE },
                ]))
            },
        }
    }
}

/// Caller CANCELs pre-18x, callee fully silent forever: crossing the
/// `NoAnswer` deadline drives NOTHING — no `/calls/failure` consult, no
/// second final on the a-leg — and the SUT's own dead-call detection (the
/// terminating safety timer) reaps the call.
#[tokio::test(start_paused = true)]
async fn no_answer_deadline_on_a_caller_cancelled_call_is_inert() {
    let h = Harness::new("no-answer-cancelled-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let consults = Arc::new(AtomicUsize::new(0));
    let b2bua = B2buaSut::builder(failover_capable_decision(5070, consults.clone()))
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── alice INVITEs through the B2BUA; bob receives but stays silent ──────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(300)).await;
    let _b_inv = bob.receive("INVITE").await; // delivered; bob never answers

    // ── alice hangs up pre-provisional ───────────────────────────────────────
    // The a-leg server txn completes at once: 200 to the CANCEL, 487 to the
    // INVITE (ACKed by the txn-layer auto-ACK) — the ONLY final it ever takes.
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── cross exactly the held-CANCEL grace deadline ─────────────────────────
    // The b-leg branch is response-less, so the CANCEL is held the full grace
    // window and then sent regardless (ADR-0028). Bob — gone dark — receives
    // it and answers nothing.
    h.advance(Duration::from_millis(sip_txn::timers::CANCEL_HOLD_GRACE + 500)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_some(),
        "the grace expiry puts the b-leg CANCEL on the wire (ADR-0028)",
    );

    // ── cross exactly the (scrubbed) NoAnswer deadline ───────────────────────
    // Armed at route time (~t0), so +NO_ANSWER_SEC from the CANCEL crosses it
    // and nothing else (the next deadline is the 32 s terminating backstop).
    h.advance(
        Duration::from_secs(NO_ANSWER_SEC as u64)
            - Duration::from_millis(sip_txn::timers::CANCEL_HOLD_GRACE + 500),
    )
    .await;
    assert_eq!(
        consults.load(Ordering::SeqCst),
        0,
        "no /calls/failure consult for an abandoned call"
    );
    // No second final reaches alice: her queue is empty (a regression's 480
    // would surface here as an unexpected response).
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    // And no SECOND CANCEL toward bob: the grace copy is sent exactly once
    // (no provisional ever arrives, so no re-flush either).
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "exactly one grace-expiry CANCEL reaches the dead callee",
    );

    // ── the SUT's own dead-call detection reaps the call ─────────────────────
    // The terminating backstop (armed at the CANCEL) is the next deadline.
    h.advance(Duration::from_millis(
        call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000
            - Duration::from_secs(NO_ANSWER_SEC as u64).as_millis() as u64,
    ))
    .await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(consults.load(Ordering::SeqCst), 0, "still no consult through teardown");
    b2bua.assert_fully_reaped();

    // The CDR records the CANCEL, and no trace of a no-answer treatment.
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert!(
        cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::Cancel),
        "the caller's CANCEL is on the CDR: {:?}",
        cdrs[0].events,
    );
    assert!(
        !reasons_of(&cdrs[0]).iter().any(|r| r.contains("no_answer") || r.contains("no-answer")),
        "no no-answer marker on the abandoned call: {:?}",
        reasons_of(&cdrs[0]),
    );

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// A service watchdog armed before the caller's CANCEL is disarmed at
/// termination: its deadline passes inside the terminating window with no
/// fire — nothing to absorb, nothing re-armed, no consult, no second final —
/// and the terminating backstop reaps the call on schedule.
#[tokio::test(start_paused = true)]
async fn a_service_watchdog_armed_before_the_cancel_is_disarmed_at_termination() {
    let h = Harness::new("no-answer-stale-terminating");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let consults = Arc::new(AtomicUsize::new(0));
    let b2bua = B2buaSut::builder(failover_capable_decision(5070, consults.clone()))
        .services(vec![stalerestore::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(300)).await;
    let _b_inv = bob.receive("INVITE").await; // delivered; bob never answers

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // Cross the held-CANCEL grace deadline first (ADR-0028): bob — gone dark —
    // receives the grace-expiry CANCEL and answers nothing.
    h.advance(Duration::from_millis(sip_txn::timers::CANCEL_HOLD_GRACE + 500)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_some(),
        "the grace expiry puts the b-leg CANCEL on the wire (ADR-0028)",
    );
    // Then cross ONLY the probe's inject deadline: disarmed at the CANCEL, it
    // never fires, so no stale per-b-leg NoAnswer entry is re-armed.
    h.advance(
        Duration::from_secs(stalerestore::INJECT_AT_SEC as u64)
            - Duration::from_millis(sip_txn::timers::CANCEL_HOLD_GRACE + 500),
    )
    .await;
    // And the deadline the probe would have armed.
    h.advance(Duration::from_secs(stalerestore::STALE_FIRE_SEC as u64 + 1)).await;

    assert_eq!(consults.load(Ordering::SeqCst), 0, "no consult on a going-away leg");
    assert_eq!(
        b2bua.metrics().going_away_absorbed_total(),
        0,
        "the probe's watchdog left the ledger at termination: nothing fired",
    );
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );

    // The terminating backstop still reaps the call on schedule.
    h.advance(Duration::from_millis(
        call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000
            - Duration::from_secs(
                (stalerestore::INJECT_AT_SEC + stalerestore::STALE_FIRE_SEC + 1) as u64,
            )
            .as_millis() as u64,
    ))
    .await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(consults.load(Ordering::SeqCst), 0, "still no consult through teardown");
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "exactly one grace-expiry CANCEL reaches the dead callee — never a second",
    );
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// The `handle-timeout` sibling of the same invariant, in-SUT: the b-leg's
/// long INVITE transaction backstop (~158 s, sip-txn) fires on a leg the
/// caller already CANCELed — carol rang 180, alice CANCELed just before the
/// backstop, and carol's txn layer answered the CANCEL but the dead app never
/// sent the 487, so the leg sits Early + `Cancelling` when the transaction
/// dies. The fire resolves the leg locally: no `/calls/failure` consult
/// (origin `transaction_timeout`), no second final on the a-leg's completed
/// transaction, no second CANCEL toward the callee — and clearing
/// `Cancelling` lets the deferred termination finalize at once instead of
/// riding the 32 s terminating backstop.
#[tokio::test(start_paused = true)]
async fn invite_transaction_timeout_on_a_caller_cancelled_call_is_inert() {
    // 1 ms transit, mirroring `cancel_200_crossing_internal`'s timeout shape.
    let h = Harness::with_transit_delay("txn-timeout-cancelled-call", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // rings once, then dies
    let consults = Arc::new(AtomicUsize::new(0));
    let consults_in = consults.clone();
    // Failover-capable (callback_context) so the consult path is REACHABLE,
    // but NO NoAnswer deadline: the sip-txn INVITE backstop is the fire under
    // test. Setup deadline past it; keepalive far out; reaper off.
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("nk068-txn".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |_req| {
                consults_in.fetch_add(1, Ordering::SeqCst);
                CallTreatment::Reject(RejectDecision {
                    reject_code: 480,
                    reject_reason: Some("Temporarily Unavailable".into()),
                    update_headers: None,
                })
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&carol).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── the caller hangs up just before the transaction backstop ────────────
    // The 180 keeps the b-leg's client transaction alive to its long timeout
    // AND lets the b-leg CANCEL go on the wire (§9.1). Carol 200s the CANCEL
    // but never sends the 487.
    h.advance(Duration::from_secs(157)).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut b_cancel = carol.receive("CANCEL").await;
    b_cancel.respond(200, "OK").await;

    // ── cross exactly the 158 s transaction backstop ─────────────────────────
    // (The terminating safety timer, armed at the CANCEL, sits at ~189 s.)
    // Pre-fix this consulted /calls/failure and relayed its 480 onto the
    // a-leg's completed transaction; now the leg resolves locally and
    // finalization promotes the call immediately.
    h.advance(Duration::from_secs(2)).await;
    assert_eq!(consults.load(Ordering::SeqCst), 0, "no consult for an abandoned call");
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    assert!(
        carol.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no second CANCEL toward the already-CANCELed callee",
    );

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(consults.load(Ordering::SeqCst), 0, "still no consult through teardown");
    b2bua.assert_fully_reaped();

    // The CDR records the CANCEL and no trace of a timeout treatment.
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert!(
        cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::Cancel),
        "the caller's CANCEL is on the CDR: {:?}",
        cdrs[0].events,
    );
    assert!(
        !reasons_of(&cdrs[0]).iter().any(|r| r.contains("transaction_timeout")),
        "no timeout treatment on the abandoned call: {:?}",
        reasons_of(&cdrs[0]),
    );

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}
