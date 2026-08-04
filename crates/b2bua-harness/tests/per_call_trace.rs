//! End-to-end: a SAMPLED call records its whole story on one root span
//! (ADR-0026).
//!
//! The scenario itself is the canonical complete callflow — alice → b2bua → bob,
//! INVITE/180/200/ACK/BYE — so the wire stays RFC-compliant and the call reaches
//! a terminal state; the trace assertions ride on top of it. This is one of the
//! dedicated tests OF the trace machinery, so it may assert on the captured
//! subscriber buffer; ordinary scenario tests never do (the `Recorder` is their
//! oracle).
//!
//! The trace registry is process-wide by construction (one root span per call
//! per process), so this file holds exactly ONE test and installs its own gate.

use std::sync::Arc;

use b2bua::trace::{install_process_traces, traces, CallTraces};
use b2bua_harness::{settle_until, B2buaScene};
use observe::{RateDraw, SampleAdmission, TokenBucket};

/// A gate that samples every call: an exporter is "configured", the draw always
/// wins, and the header is not honored (this process did not opt in).
fn sample_everything() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(1), TokenBucket::default_at(0)),
        false,
    )));
}

#[tokio::test]
async fn a_sampled_call_records_its_messages_decision_and_rules_on_one_span() {
    sample_everything();
    let (_log_guard, log) = observe::test_buffer();

    let s = B2buaScene::new("per-call-trace").await;
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the scenario is a complete call");

    // ── The call's story ────────────────────────────────────────────────────
    let sip_in = log.matching("kind=sip.in");
    assert!(
        sip_in.iter().any(|e| e.contains("INVITE sip:")),
        "the INVITE as it arrived, raw: {:?}",
        log.matching("kind=sip.in").len()
    );
    assert!(
        !log.matching("kind=sip.out").is_empty(),
        "the messages the b2bua put on the wire"
    );
    assert!(
        log.matching("kind=sip.out").iter().any(|e| e.contains("100 Trying")),
        "the transaction layer's auto-100 is part of the story"
    );
    assert!(
        log.matching("kind=http.request").iter().any(|e| e.contains("/call/new")),
        "the decision round trip is a child span carrying both bodies"
    );
    assert!(
        log.matching("kind=http.response").iter().any(|e| e.contains("detail=route")),
        "…including the treatment the engine returned"
    );
    assert!(!log.matching("kind=rule.fired").is_empty(), "the rules that handled each event");

    // Facts carry the timestamp of the fact, not of the emission.
    assert!(sip_in.iter().all(|e| e.contains("at_ms=")), "every fact is timestamped");

    // ── Release ─────────────────────────────────────────────────────────────
    s.b2bua.assert_fully_reaped();
    assert_eq!(traces().active(), 0, "the root span closed with the call, freeing its slot");

    s.finish().await;
}
