//! ADR-0020 — the call reaper end-to-end: every in-process escape route from
//! the "released exactly once, one CDR" promise is closed.
//!
//! Matrix rows covered here (the ledger semantics — monotonic stamps,
//! takeover exclusion, reclaim re-stamping — are unit-pinned in
//! `b2bua/tests/reaper_ledger.rs`; the verdict confirm matrix incl. the
//! no-resurrection rule is unit-pinned in `b2bua::reaper::tests`):
//! - **stale Active call** (lost timers / dropped events) → swept + reaped;
//! - **handler panic, strike 1** → `fatal-error` verdict through the normal
//!   rules;
//! - **handler panic, strike 2** (the rules path itself panics) → discharge
//!   bypass, still exactly one CDR through the one enforce funnel;
//! - **wedged handler holding the per-call lock** (a hung decision await) →
//!   sweep escalation aborts the body, the queued verdict reaps;
//! - every row asserts **exactly one CDR** and the full reap oracle
//!   (creations == removals, no live call, no lock, no stamp).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::rules::{
    Match, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef, ServiceSeed,
    SERVICE_LAYER,
};
use b2bua_harness::{establish, settle_until, B2buaSut, OFFER_SDP};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

// ── probe service: rules that panic on demand ───────────────────────────────

fn no_init(_: &RuleCall) -> Option<ServiceSeed> {
    None
}

/// SERVICE_LAYER rule that panics on any in-dialog INFO (outranks the CORE
/// `relay-info`) — the strike-1 trigger.
fn panic_on_info(_: &RuleContext) -> Option<RuleHandleResult> {
    panic!("probe: handler panic on INFO");
}
fn panic_on_info_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "probe-panic-on-info",
        SERVICE_LAYER,
        &[],
        Match::request().method("INFO"),
        panic_on_info,
    )]
}
fn panic_on_info_service() -> ServiceDef {
    ServiceDef { id: "probe-panic-info", init: no_init, rules: panic_on_info_rules }
}

/// SERVICE_LAYER rule that panics on the reaper's `fatal-error` verdict
/// (outranks the CORE `reaper-fatal-error`) — makes the rules path itself fail
/// so the second strike must discharge.
fn panic_on_fatal(_: &RuleContext) -> Option<RuleHandleResult> {
    panic!("probe: rules path broken for fatal-error");
}
fn panic_on_fatal_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "probe-panic-on-fatal",
        SERVICE_LAYER,
        &[],
        Match::internal_event().topic("reaper").outcome("fatal-error"),
        panic_on_fatal,
    )]
}
fn panic_on_fatal_service() -> ServiceDef {
    ServiceDef { id: "probe-panic-fatal", init: no_init, rules: panic_on_fatal_rules }
}

/// A decision engine that never answers `new_call` — the hung-handler shape
/// (in production: a dead HTTP backend). The body parks INSIDE the per-call
/// FIFO worker, under the per-call lock.
struct NeverDecide;

#[async_trait]
impl CallDecisionEngine for NeverDecide {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        std::future::pending().await
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        std::future::pending().await
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        std::future::pending().await
    }
}

fn reason_of(cdr: &b2bua::cdr::CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

// ── 1. stale Active call (lost timers / dropped events) ─────────────────────

#[tokio::test(start_paused = true)]
async fn stale_active_call_is_swept_and_reaped() {
    let h = Harness::with_transit_delay("reaper-stale-active", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            // Simulate the lost-timer class: no keepalive inside the horizon (its
            // interval is pushed out), and an explicit short idle threshold.
            c.keepalive_interval_sec = 3_600;
            c.reaper_idle_max_sec = 60;
            c.reaper_sweep_interval_sec = 30;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // Establish, then both UAs go silent forever (their timers "lost").
    let _dialog = establish(&alice, &bob, b2bua.addr).await;

    // Advance past idle_max + one sweep: 30/60 sweeps see idle < 60 s; the 90 s
    // sweep proves staleness and injects the verdict.
    h.advance(Duration::from_secs(95)).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR for the reaped call");
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "reaper-stale"),
        "the CDR names the reap: {:?}",
        reason_of(&cdrs[0])
    );
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert!(b2bua.metrics().reaper_verdicts_total() >= 1);
    assert_eq!(b2bua.metrics().reaper_discharged_total(), 0, "rules path was healthy");

    // The reap is wire-silent (the call was provably dead); the UAs simply
    // never hear a BYE — accepted, the CDR disposition names it.
    let _report = h.finish().await;
}

// ── 2. handler panic, strike 1 → fatal-error verdict via the rules ──────────

#[tokio::test]
async fn handler_panic_strike1_reaps_via_rules() {
    let h = Harness::with_transit_delay("reaper-panic-strike1", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![panic_on_info_service()])
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut dialog = establish(&alice, &bob, b2bua.addr).await;

    // The INFO trips the panicking probe rule: pre-ADR-0020 this leaked the
    // call forever with zero CDR; now it is strike 1 → fatal-error → reaped.
    let _txn = dialog.request(InDialogMethod::Info, None).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR despite the panic");
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "handler-panic"),
        "the CDR names the panic: {:?}",
        reason_of(&cdrs[0])
    );
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.metrics().handler_panics_total(), 1);
    assert_eq!(b2bua.metrics().reaper_discharged_total(), 0, "strike 1 stays on the rules path");

    let _report = h.finish().await;
}

// ── 3. handler panic, strike 2 (rules path broken) → discharge ──────────────

#[tokio::test]
async fn second_panic_discharges_outside_the_rules() {
    let h = Harness::with_transit_delay("reaper-panic-strike2", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let b2bua = B2buaSut::builder(decision)
        // The INFO panics (strike 1) AND the fatal-error verdict's rule panics
        // too (strike 2) — the rules path is broken for this call, so only the
        // discharge bypass can save the CDR.
        .services(vec![panic_on_info_service(), panic_on_fatal_service()])
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut dialog = establish(&alice, &bob, b2bua.addr).await;
    let _txn = dialog.request(InDialogMethod::Info, None).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR even with the rules path broken");
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "reaper-discharge"),
        "the CDR names the discharge: {:?}",
        reason_of(&cdrs[0])
    );
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.metrics().handler_panics_total(), 2, "both panics observed");
    assert_eq!(b2bua.metrics().reaper_discharged_total(), 1, "the strike-2 bypass fired");

    let _report = h.finish().await;
}

// ── 4. wedged handler holding the per-call lock → abort + reap ──────────────

#[tokio::test(start_paused = true)]
async fn wedged_setup_is_aborted_and_reaped() {
    let h = Harness::with_transit_delay("reaper-wedged-setup", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(Arc::new(NeverDecide))
        .tune(|c| {
            c.reaper_idle_max_sec = 60;
            c.reaper_sweep_interval_sec = 30;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let _ = bob; // never reached: the decision wedges before any b-leg exists

    // The INVITE parks the per-call worker (and its lock) inside the
    // never-resolving decision await. The txn layer's 100 Trying is all alice
    // ever hears.
    let _call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;

    // Sweeps: verdicts at 90/120 s queue behind the parked body; the 150 s
    // sweep (attempt 3) aborts the in-flight body — the lock releases, the
    // queued verdict runs, the call reaps.
    h.advance(Duration::from_secs(160)).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR for the wedged call");
    let reasons = reason_of(&cdrs[0]);
    assert!(
        reasons.iter().any(|r| r == "reaper-stale" || r == "handler-panic"),
        "the CDR names the forced reap: {reasons:?}"
    );
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert!(b2bua.metrics().reaper_verdicts_total() >= 3, "escalation ladder ran");

    let _report = h.finish().await;
}
