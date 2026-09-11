//! HA: a takeover of a SAMPLED call opens the survivor's OWN root span, linked
//! to the nominal's (ADR-0026 §5).
//!
//! Drives the real materialisation seam — the acting-backup's `router::materialise`
//! on a failed-over in-dialog BYE — rather than the `adopt_into` unit seam, so a
//! regression that drops the adoption, or runs it on a copy the store discards,
//! fails here.
//!
//! The scenario is a complete callflow: alice ⇄ b2bua ⇄ bob established through
//! the proxy over a genuine limiter hold, the primary crashes, alice's BYE fails
//! over to the backup, and the cluster settles to no-trace-anywhere. StayDead
//! (Model Y, ADR-0020 X3): the crashed primary is the sole CDR authority, so this
//! call's CDR is the accepted loss and the backup's lossy cleanup is what
//! releases the limiter and frees the replica.
//!
//! The trace registry is process-wide (one root span per call per process), so
//! this file holds exactly ONE test and installs its own gate. It is a test OF
//! the trace machinery, so it may assert on the captured subscriber buffer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::trace::{install_process_traces, CallTraces};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use failover_harness::{
    assert_call_lost_no_cdr, worker_ordinals, FailoverHarness, ReplicatedB2buaSut, WorkerHealth,
};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use observe::{CapturedSpan, RateDraw, SampleAdmission, TokenBucket};
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// A gate that samples every call: an exporter is "configured", the draw always
/// wins, and the header is not honored (this process did not opt in).
fn sample_everything() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(1), TokenBucket::default_at(0)),
        false,
    )));
}

/// Route every call to bob holding one slot on trunk-A, so the takeover node's
/// lossy cleanup has a real hold to release.
fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-A".into(), limit: 8 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        LIMITER_ADDR.parse::<SocketAddr>().expect("limiter addr"),
        Duration::from_millis(150),
    ))
}

/// The value of `field` on a captured span, or `""`.
fn field<'a>(span: &'a CapturedSpan, name: &str) -> &'a str {
    span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or_default()
}

/// The root spans opened for calls (`sip.call`), oldest first — HTTP child spans
/// excluded.
fn root_spans(log: &observe::TestLogHandle) -> Vec<CapturedSpan> {
    log.spans().into_iter().filter(|s| s.name == "sip.call").collect()
}

#[tokio::test(start_paused = true)]
async fn a_takeover_opens_this_nodes_own_root_span_linked_to_the_nominals() {
    sample_everything();
    let (_log_guard, log) = observe::test_buffer();

    let mut fh = FailoverHarness::new("takeover-linked-span", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // The shared limiter lives outside the workers, so it survives the crash.
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http
        .serve(LIMITER_ADDR.parse().expect("limiter addr"), server)
        .await
        .expect("limiter server");

    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let decision = limited_decision();
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            decision.clone(),
            limiter_client(&http),
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
            decision.clone(),
            limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── Establish alice ⇄ bob on the HRW primary ─────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, bak_ord) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Drive establish + replicate-on-flush (primary → backup).
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "the call is admitted: one limiter hold");

    let mut call_ref = String::new();
    {
        let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
        assert_eq!(bak_ord, backup.ordinal(), "cookie w_bak names the backup worker");
        for _ in 0..50 {
            if let Some(rf) = backup.scan_one_backed_up(&primary_ord).await {
                call_ref = rf;
                break;
            }
            fh.advance(Duration::from_millis(100)).await;
        }
    }
    assert!(!call_ref.is_empty(), "the backup holds the replicated call");

    // ── The nominal's root span ──────────────────────────────────────────────
    let nominal = {
        let spans = root_spans(&log);
        assert_eq!(spans.len(), 1, "the primary opened exactly one root span for the call");
        spans.into_iter().next().expect("the nominal root span")
    };
    assert!(!field(&nominal, "trace_id").is_empty(), "the call is sampled and traced");
    assert_eq!(field(&nominal, "link.span_id"), "", "a first-hand call links nothing");

    // ── Crash the primary; the proxy fails alice's dialog over to the backup ─
    // From here the leg has two potential owners, so ADR-0014's accepted
    // keepalive-vs-backup-transaction overlap may reuse an in-dialog CSeq (one call
    // drops cleanly). Accepted for the takeover window only — establishment above
    // keeps `cseq-in-dialog-order` fully gating.
    fh.accept_rfc_deviations_from_now(
        failover_harness::RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0014 accepted trade-off: dual-owner in-dialog CSeq overlap in the \
         takeover window — one call drops cleanly",
    );
    fh.mark(&primary_ord, None, "crash", "primary down");
    let hydrated_before = {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        primary.crash();
        proxy.set_health(&primary_ord, WorkerHealth::Dead);
        backup.simulate_peer_removed(&primary_ord);
        backup.metrics().repl_takeover_hydrated_total()
    };
    fh.advance(Duration::from_millis(300)).await;

    // ── The BYE takes the call over on the backup and terminates it ──────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    {
        let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
        assert!(
            backup.metrics().repl_takeover_hydrated_total() > hydrated_before,
            "the BYE hydrated the call onto the acting-backup (the takeover seam ran)",
        );
    }

    // ── GATE: the backup serves the call under its OWN root span, LINKED ─────
    let takeover = {
        let spans = root_spans(&log);
        assert_eq!(spans.len(), 2, "the takeover opened a second root span: {spans:?}");
        spans.into_iter().nth(1).expect("the takeover root span")
    };
    assert_eq!(
        field(&takeover, "trace_id"),
        field(&nominal, "trace_id"),
        "one trace spans both processes",
    );
    assert_ne!(
        field(&takeover, "span_id"),
        field(&nominal, "span_id"),
        "the backup serves the call under its OWN root span",
    );
    assert_eq!(
        field(&takeover, "link.span_id"),
        field(&nominal, "span_id"),
        "…carrying a LINK to the nominal's root, never a parent",
    );
    assert_eq!(
        field(&takeover, "sip.call_id"),
        field(&nominal, "sip.call_id"),
        "the Call-ID is the only cross-process correlation key",
    );
    assert!(
        log.matching("kind=sip.in").iter().any(|e| e.contains("BYE sip:")),
        "the failed-over BYE recorded on the takeover node's span",
    );

    // ── Release: the StayDead contract — limiter back to 0, nothing left ─────
    let _ = fh
        .settle_lossy_cleanup(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_lost_no_cdr(&[&w_b1, &w_b2], &call_ref, &store).await;

    drop((w_b1, w_b2, proxy));
}
