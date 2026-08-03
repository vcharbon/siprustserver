//! End-to-end: a transition the invariant layer synthesizes reaches the trace
//! (ADR-0026).
//!
//! A rule's turn does not end when its actions run. `invariants::finalize`
//! folds a fully-resolved `Terminating` call to `Terminated`, and
//! `invariants::enforce` then answers the caller the ADR-0022 `503` for an a-leg
//! that reaches termination unanswered. Recording the turn's transitions before
//! those layers ran left a traced call showing the 503 leaving the box with no
//! transition explaining it — the one question the trace is opened to answer.
//!
//! The scenario is `setup_stall_global_duration_reap`'s: bob rings and goes
//! silent, so the call sits `Active` with its a-leg unanswered until the
//! GlobalDuration cap reaps it. That teardown is exactly the case where the
//! terminal fold and the synthesized 503 both come from the invariant layer.
//!
//! The trace registry is process-wide, so this file holds exactly ONE test and
//! installs its own gate.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua::trace::{install_process_traces, traces, CallTraces};
use b2bua_harness::{settle_until, B2buaSut};
use observe::{RateDraw, SampleAdmission, TokenBucket};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The absolute duration cap, deliberately below the b-leg INVITE client
/// transaction's Timer B (32 s) so the GlobalDuration backstop — not a txn
/// timeout — is what tears the stalled call down.
const MAX_DURATION_SEC: i64 = 20;
const MAX_DURATION: Duration = Duration::from_secs(MAX_DURATION_SEC as u64);

/// A gate that samples every call.
fn sample_everything() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(2), TokenBucket::default_at(0)),
        false,
    )));
}

#[tokio::test(start_paused = true)]
async fn the_enforced_teardown_of_an_unanswered_a_leg_shows_on_the_trace() {
    sample_everything();
    let (_log_guard, log) = observe::test_buffer();

    let h = Harness::new("b2bua-trace-enforced-transition");
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut route = route_to("127.0.0.1", 5074);
                route.features.platform.max_duration_sec = MAX_DURATION_SEC;
                NewCallResponse::Route(route)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5084").await;

    // ── A setup that stalls: bob rings, then never answers ───────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── The cap fires: the b-leg is CANCELed, and the still-unanswered a-leg
    //    gets the ADR-0022 synthesized 503 ─────────────────────────────────────
    h.advance(MAX_DURATION + Duration::from_secs(1)).await;
    let mut cancel = bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    call.expect(503).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    // ── The synthesized final left the box… ──────────────────────────────────
    let out = log.matching("kind=sip.out");
    assert!(
        out.iter().any(|e| e.contains("SIP/2.0 503")),
        "the enforced final response is on the trace: {:?}",
        out.iter().map(|e| e.line()).collect::<Vec<_>>()
    );

    // ── …and so did the transition that produced it ──────────────────────────
    let transitions = log.matching("kind=call.transition");
    assert!(
        transitions.iter().any(|e| e.contains("-> Terminated")),
        "the terminal fold is synthesized by `invariants::finalize`, AFTER the \
         rule's actions ran — recording the turn before it leaves the 503 \
         unexplained: {:?}",
        transitions.iter().map(|e| e.line()).collect::<Vec<_>>()
    );
    assert!(
        log.matching("kind=rule.transition")
            .iter()
            .any(|e| e.contains("global-call") && e.contains("-> Terminated")),
        "the global-call machine cursor the fold projects moves with it"
    );

    assert_eq!(traces().active(), 0, "the root span closed with the call");

    let _report = h.finish().await;
}
