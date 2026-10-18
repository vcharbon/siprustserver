//! ADR-0020 — the call reaper end-to-end: every in-process escape route from
//! the "released exactly once, one CDR" promise is closed.
//!
//! Matrix rows covered here (the ledger semantics — monotonic stamps,
//! takeover exclusion, reclaim re-stamping — are unit-pinned in
//! `b2bua/tests/it/reaper_ledger.rs`; the verdict confirm matrix incl. the
//! no-resurrection rule is unit-pinned in `b2bua::reaper::tests`):
//! - **stale Active call** (lost timers / dropped events) → swept + reaped;
//! - **handler panic, strike 1** → `fatal-error` verdict through the normal
//!   rules, the live legs torn down on the wire (BYE to a confirmed leg,
//!   CANCEL to an early b-leg, the caller's owed final);
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
use b2bua_harness::{establish, settle_until, B2buaScene, B2buaSut, OFFER_SDP};
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
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert!(b2bua.metrics().reaper_verdicts_total() >= 1);
    assert_eq!(b2bua.metrics().reaper_discharged_total(), 0, "rules path was healthy");

    // The reap is wire-silent (the call was provably dead); the UAs simply
    // never hear a BYE — accepted, the CDR disposition names it.
    assert!(alice.take_queued().await.is_none(), "the stale reap sends alice nothing");
    assert!(bob.take_queued().await.is_none(), "the stale reap sends bob nothing");
    let _report = h.finish().await;
}

// ── 2. handler panic, strike 1 → fatal-error verdict, torn down on the wire ─

/// SERVICE_LAYER rule that panics on the callee's 183 — the strike-1 trigger
/// while the b-leg is early.
fn panic_on_183(_: &RuleContext) -> Option<RuleHandleResult> {
    panic!("probe: handler panic on 183");
}
fn panic_on_183_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "probe-panic-on-183",
        SERVICE_LAYER,
        &[],
        Match::response().method("INVITE").status_code(183),
        panic_on_183,
    )]
}
fn panic_on_183_service() -> ServiceDef {
    ServiceDef { id: "probe-panic-183", init: no_init, rules: panic_on_183_rules }
}

/// alice's INFO kills its handler on a confirmed call. Strike 1 tears the
/// call down on the wire: a BYE to each leg, each answered 200, and one CDR
/// naming the panic. A copy of the INFO while the BYEs are out is absorbed by
/// its server transaction and never re-enters the rule; the copy after the
/// release draws 481.
#[tokio::test(start_paused = true)]
async fn handler_panic_strike1_sends_bye_to_both_legs_of_a_confirmed_call() {
    let s = B2buaScene::with_b2bua("reaper-panic-strike1-confirmed", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).services(vec![panic_on_info_service()])
    })
    .await;
    let mut dialog = s.establish().await;

    let (mut info, info_req) = dialog
        .send_request(InDialogMethod::Info)
        .try_send_with_request()
        .await
        .expect("the INFO leaves");

    let mut a_bye = s.alice.try_receive("BYE").await.expect("strike 1 sends alice a BYE");
    let mut b_bye = s.bob.try_receive("BYE").await.expect("strike 1 sends bob a BYE");
    s.alice.try_send_datagram(info_req.image(), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1, "the INFO's copy ran no handler");
    a_bye.respond(200, "OK").await;
    b_bye.respond(200, "OK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR despite the panic");
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "handler-panic"),
        "the CDR names the panic: {:?}",
        reason_of(&cdrs[0])
    );
    assert_eq!(s.b2bua.metrics().reaper_discharged_total(), 0, "strike 1 stays on the rules path");

    s.alice.try_send_datagram(info_req.image(), s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_millis(300)).await;
    info.try_expect(481).await.expect("the released call answers the INFO's copy 481");
    let _ = s.finish().await;
}

const REOFFER: &str = "v=0\r\no=alice 101 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 202 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// Set once the re-INVITE 2xx probe has panicked: the defect it stands for
/// strikes one turn, not every copy of the 2xx.
static REINVITE_2XX_PANICKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// SERVICE_LAYER rule that panics on the callee's first 2xx to a re-INVITE
/// (a CSeq past the initial INVITE's): the panicking turn consumed that 2xx.
fn panic_on_reinvite_2xx(ctx: &RuleContext) -> Option<RuleHandleResult> {
    use std::sync::atomic::Ordering;
    if ctx.response().is_some_and(|r| r.cseq().seq() > 1)
        && !REINVITE_2XX_PANICKED.swap(true, Ordering::SeqCst)
    {
        panic!("probe: handler panic on a re-INVITE's 2xx");
    }
    None
}
fn panic_on_reinvite_2xx_service() -> ServiceDef {
    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition::core(
            "probe-panic-on-reinvite-2xx",
            SERVICE_LAYER,
            &[],
            Match::response().method("INVITE").status_class(2),
            panic_on_reinvite_2xx,
        )]
    }
    ServiceDef { id: "probe-panic-reinvite-2xx", init: no_init, rules }
}

/// bob's 2xx to the re-INVITE alice made kills its handler before it is
/// relayed, so the call never learns of it. Strike 1 tears the call down on
/// the wire: the relayed re-INVITE is CANCELled (bob's transaction already
/// has its final, so no CANCEL leaves), alice's re-INVITE is answered 487,
/// and both legs are sent a BYE. bob's 2xx retransmission (RFC 3261
/// §13.3.1.4) meets the CANCELled relay and is ACKed (§13.2.2.4), and one
/// CDR names the panic.
#[tokio::test(start_paused = true)]
async fn handler_panic_strike1_on_a_reinvite_2xx_acks_its_retransmission() {
    let s = B2buaScene::with_b2bua("reaper-panic-strike1-reinvite-2xx", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port)
            .services(vec![panic_on_reinvite_2xx_service()])
    })
    .await;
    let mut dialog = s.establish().await;

    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinvite = s.bob.receive("INVITE").await;
    bob_reinvite.respond(200, "OK").with_sdp(REANSWER).await;

    let mut b_bye = s.bob.try_receive("BYE").await.expect("strike 1 sends bob a BYE");
    // bob's 2xx retransmission (RFC 3261 §13.3.1.4, at T1).
    s.h.advance(Duration::from_millis(500)).await;
    bob_reinvite.respond(200, "OK").with_sdp(REANSWER).await;
    let ack = s.bob.try_receive("ACK").await.expect("bob's 2xx is ACKed");
    assert_eq!(ack.request().cseq().seq(), bob_reinvite.request().cseq().seq());
    b_bye.respond(200, "OK").await;
    reinvite.try_expect(487).await.expect("alice's re-INVITE is answered 487");
    s.alice.try_receive("BYE").await.expect("strike 1 sends alice a BYE").respond(200, "OK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR despite the panic");
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "handler-panic"),
        "the CDR names the panic: {:?}",
        reason_of(&cdrs[0])
    );
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    let _ = s.finish().await;
}

/// bob's 183 kills its handler while his INVITE rings. Strike 1 tears the
/// call down on the wire: bob is CANCELled and his INVITE ends 487, alice is
/// answered 503, and one CDR names the panic.
#[tokio::test(start_paused = true)]
async fn handler_panic_strike1_cancels_an_early_callee_and_answers_the_caller() {
    let s = B2buaScene::with_b2bua("reaper-panic-strike1-early", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).services(vec![panic_on_183_service()])
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(183, "Session Progress").await;

    let mut cancel = s.bob.try_receive("CANCEL").await.expect("strike 1 CANCELs bob's INVITE");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    call.try_expect_final(503).await.expect("alice is answered the teardown's 503");

    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR despite the panic");
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "handler-panic"),
        "the CDR names the panic: {:?}",
        reason_of(&cdrs[0])
    );
    assert_eq!(s.b2bua.metrics().handler_panics_total(), 1);
    assert_eq!(s.b2bua.metrics().reaper_discharged_total(), 0, "strike 1 stays on the rules path");
    let _ = s.finish().await;
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
    settle_until(|| b2bua.is_reaped()).await;
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
            // Disable the ADR-0022 decision deadline: this test exercises the
            // reaper's abort-escalation ladder, which needs a GENUINELY wedged
            // decision await (with the deadline on, NeverDecide would simply
            // 503 at 5 s and the ladder would never engage — that fast path is
            // `decision_deadline.rs`'s subject).
            c.call_control_timeout_ms = 0;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let _ = bob; // never reached: the decision wedges before any b-leg exists

    // The INVITE parks the per-call worker (and its lock) inside the
    // never-resolving decision await. Until the reap lands, the txn layer's
    // 100 Trying is all alice hears.
    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;

    // Sweeps: verdicts at 90/120 s queue behind the parked body; the 150 s
    // sweep (attempt 3) aborts the in-flight body — the lock releases, the
    // queued verdict runs, the call reaps.
    h.advance(Duration::from_secs(160)).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR for the wedged call");
    // The stale verdicts were queued behind the body, so the first of them
    // reaps the call before the abort's own strike-1 verdict arrives: that one
    // finds the call gone.
    let reasons = reason_of(&cdrs[0]);
    assert!(reasons.iter().any(|r| r == "reaper-stale"), "the stale verdict reaps: {reasons:?}");
    assert!(
        !reasons.iter().any(|r| r == "handler-panic"),
        "the abort's strike runs after the reap: {reasons:?}"
    );
    // ADR-0022: the reap is not silent toward the caller — the
    // `→ terminated` funnel answers the still-unanswered a-leg INVITE with a
    // 503 through its (still-live, < 193 s) server txn. Late, but alice is
    // released instead of stranded on 100-then-silence.
    call.expect(503).await;
    assert!(
        reasons.iter().any(|r| r == "unanswered_at_termination"),
        "the CDR records the synthesized final: {reasons:?}"
    );
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert!(b2bua.metrics().reaper_verdicts_total() >= 3, "escalation ladder ran");

    let _report = h.finish().await;
}
