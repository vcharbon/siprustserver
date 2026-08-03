//! End-to-end: a `/call/failure` route can turn a call's trace on, mid-call
//! (ADR-0026 §3).
//!
//! The engine force-enable is not a `/call/new`-only privilege. The operational
//! case is "this call is failing over, follow it": the limiter refuses the
//! primary route, the b2bua consults `/call/failure`, and the failover route
//! comes back carrying `trace: true`. Honoring it only on the first response
//! makes that decision unreachable — the call routes to the failover
//! destination with no span at all.
//!
//! The configured rate is zero, so nothing here is drawn: every fact recorded
//! below exists because the failover route asked for it. The backfill still owes
//! the whole story — the INVITE the call arrived on, the auto-100 already sent,
//! and the consult that turned tracing on.
//!
//! The trace registry is process-wide, so this file holds exactly ONE test and
//! installs its own gate.

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallFailureResponse, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, LimiterHold};
use b2bua::trace::{install_process_traces, traces, CallTraces};
use b2bua_harness::{settle_until, B2buaSut};
use observe::{RateDraw, SampleAdmission, TokenBucket};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The limiter id the primary route holds and this limiter always refuses.
const TRUNK: &str = "trunk-full";

/// A limiter at its cap: every admit is refused, so the primary route always
/// fails over. Release/refresh are no-ops — a refused call holds nothing.
struct FullLimiter;

#[async_trait]
impl CallLimiter for FullLimiter {
    async fn admit(&self, _entries: &[LimiterEntry]) -> AdmitOutcome {
        AdmitOutcome::Rejected { limiter_id: TRUNK.to_string() }
    }
    async fn release(&self, _holds: &[LimiterHold]) {}
    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
        holds.to_vec()
    }
}

/// A gate whose configured rate is zero: no draw ever wins, so only a
/// force-enable can open a span.
fn sample_nothing_by_default() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 0.0, 200, RateDraw::seeded(5), TokenBucket::default_at(0)),
        false,
    )));
}

#[tokio::test]
async fn a_failover_route_turns_the_trace_on_and_backfills_the_call() {
    sample_nothing_by_default();
    let (_log_guard, log) = observe::test_buffer();

    let h = Harness::with_transit_delay("b2bua-trace-failover-force-enable", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    // Primary: a limited trunk (always refused) with a callback context, so the
    // reject consults `/call/failure`. The failover route asks for the trace.
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5073);
                r.call_limiter = vec![CallLimiterEntry { id: TRUNK.into(), limit: 1 }];
                r.callback_context = Some("trace-on-failover".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_req| {
                let mut r = route_to("127.0.0.1", 5073);
                r.trace = true;
                CallFailureResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(Arc::new(FullLimiter))
        .start(&h, "b2bua", "127.0.0.1:5083")
        .await;

    // ── The complete callflow — the limiter reject is invisible to alice ─────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "the scenario is a complete call");

    // ── The trace exists at all, and it starts at the INVITE ─────────────────
    let arrived = log.matching("kind=sip.in");
    assert!(
        arrived.iter().any(|e| e.contains("INVITE sip:")),
        "the INVITE the call arrived on is backfilled, though it long predates \
         the failover decision that turned tracing on"
    );
    assert!(
        log.matching("kind=sip.out").iter().any(|e| e.contains("100 Trying")),
        "so is the auto-100 the transaction layer had already sent"
    );

    // ── …including the consult that caused the activation ────────────────────
    assert!(
        log.matching("kind=http.request").iter().any(|e| e.contains("detail=/call/failure")),
        "the failover consult is a child span carrying its request body"
    );
    assert!(
        log.matching("kind=http.response").iter().any(|e| e.contains("detail=route")),
        "…and the treatment that carried `trace: true`"
    );
    // The `/call/new` consult ran while the call was unsampled and nothing was
    // buffered for it — the design refuses to pay for calls it will not trace.
    assert!(
        !log.matching("kind=http.request").iter().any(|e| e.contains("detail=/call/new")),
        "nothing is buffered for an unsampled call; only the backfill is owed"
    );

    // ── …and everything after it ─────────────────────────────────────────────
    assert!(!log.matching("kind=rule.fired").is_empty(), "the call records once traced");

    b2bua.assert_fully_reaped();
    assert_eq!(traces().active(), 0, "the root span closed with the call");

    let _report = h.finish().await;
}
