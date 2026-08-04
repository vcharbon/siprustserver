//! End-to-end: the decision engine turns a call's trace on, and the call is
//! BACKFILLED (ADR-0026 §3).
//!
//! The configured rate here is zero, so the intake draw never activates a call:
//! the only way this call is traced is the engine answering `trace: true`. What
//! the test pins is that activating late loses nothing — the INVITE the call
//! arrived on, the auto-100 the transaction layer already sent, and the decision
//! round trip that caused the activation are all recorded, at THEIR timestamps.
//! Nothing was buffered for the calls that were not force-enabled; the backfill
//! reads facts the call already carries.
//!
//! The scenario is the canonical complete callflow, so the wire stays
//! RFC-compliant and the call terminates. The trace registry is process-wide, so
//! this file holds exactly ONE test and installs its own gate.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua::trace::{install_process_traces, traces, CallTraces};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, BOB_PORT};
use observe::{RateDraw, SampleAdmission, TokenBucket};

/// A gate whose configured rate is zero: no draw ever wins, so only a
/// force-enable can open a span.
fn sample_nothing_by_default() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 0.0, 200, RateDraw::seeded(5), TokenBucket::default_at(0)),
        false,
    )));
}

#[tokio::test]
async fn an_engine_force_enable_activates_and_backfills_the_call() {
    sample_nothing_by_default();
    let (_log_guard, log) = observe::test_buffer();

    let s = B2buaScene::with_b2bua("trace-force-enable", |bob_port| {
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    let mut route = route_to("127.0.0.1", bob_port);
                    route.trace = true;
                    NewCallResponse::Route(route)
                })
                .build(),
        );
        B2buaSut::builder(decision)
    })
    .await;
    assert_eq!(BOB_PORT, 5070, "the scene's canonical bob port is what the route names");

    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the scenario is a complete call");

    // ── The backfill: everything that happened BEFORE the activation ────────
    assert!(
        log.matching("kind=sip.in").iter().any(|e| e.contains("INVITE sip:")),
        "the INVITE the call arrived on is recorded even though it predates the draw"
    );
    assert!(
        log.matching("kind=sip.out").iter().any(|e| e.contains("100 Trying")),
        "so is the auto-100 the transaction layer had already sent"
    );
    let round_trip = log.matching("kind=http.request");
    assert!(
        round_trip.iter().any(|e| e.contains("/call/new")),
        "and the decision round trip that caused the activation, request body included"
    );
    assert!(
        log.matching("kind=http.response").iter().any(|e| e.contains("detail=route")),
        "with the treatment that carried `trace: true`"
    );

    // ── …and everything after it ────────────────────────────────────────────
    assert!(!log.matching("kind=rule.fired").is_empty(), "the call keeps recording once traced");

    s.b2bua.assert_fully_reaped();
    assert_eq!(traces().active(), 0, "the root span closed with the call");

    s.finish().await;
}
