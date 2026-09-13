//! The decision seam's call context + failover parity:
//!
//!   1. `/call/failure` carries the framework-attached [`CallSnapshot`]
//!      (observed CDR trail, legs, features, limiter holds) plus the failed
//!      final response's non-structural headers — the generic carrier a real
//!      decision backend derives `prov18x` / Q.850 causes / anything else from.
//!   2. A **pending b-leg INVITE transaction timeout** (the dead-gateway case)
//!      consults `/call/failure` (origin `transaction_timeout`) instead of
//!      unconditionally terminating, so the backend can reroute — and the
//!      consult says which timeout fired (`timeout_kind`): `response` when the
//!      hop answered nothing at all (the tightened first-response bound,
//!      ADR-0032 X1 amendment), `transaction` when it rang and went silent.
//!   3. A failover **Route is honored like an initial route**: its
//!      `call_limiter` entries are admitted against the reroute target and the
//!      holds are released at termination; a limiter reject on the failover
//!      route re-consults `/call/failure` (origin `call_limiter`), bounded.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallFailureRequest, CallLimiterEntry, CallTreatment, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

async fn serve_limiter(
    net: &SimulatedHttpNetwork,
) -> (Arc<WindowStore>, Box<dyn HttpServerHandle>) {
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (store, handle)
}

fn limiter_client(net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(Arc::new(net.clone()), laddr(), Duration::from_millis(150)))
}

/// Shared capture slot for the `/call/failure` requests an engine sees.
type Captured = Arc<Mutex<Vec<CallFailureRequest>>>;

// ── 1. Snapshot + failed-response headers on the failure request ────────────

#[tokio::test]
async fn failure_request_carries_snapshot_and_failed_response_headers() {
    let h = Harness::with_transit_delay("failure-snapshot", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                // A callback context is what makes the failure consult the
                // engine instead of relaying immediately.
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-snapshot".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                cap.lock().unwrap().push(req.clone());
                CallTreatment::Relay
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here")
        .with_header("Reason", "Q.850;cause=17")
        .with_header("X-Failure-Info", "trunk-a-congested")
        .await;
    bob.receive("ACK").await; // b2bua ACKs the failed b-leg INVITE

    // Relay declined the failover → the 486 reaches alice and the call ends.
    call.expect(486).await;
    settle_until(|| !captured.lock().unwrap().is_empty()).await;

    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 1, "one failure consult");
    let req = &reqs[0];

    // Event-scoped facts from the emitting site.
    assert_eq!(req.failure.origin, "external");
    assert_eq!(req.failure.status_code, Some(486));
    assert_eq!(req.failure.failed_leg_id.as_deref(), Some("b-1"));
    let reason = req
        .failure
        .sip_headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("reason"))
        .map(|(_, v)| v.as_str());
    assert_eq!(reason, Some("Q.850;cause=17"), "Reason header forwarded verbatim");
    assert!(
        req.failure
            .sip_headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("x-failure-info") && v == "trunk-a-congested"),
        "X-* headers come along at no extra cost: {:?}",
        req.failure.sip_headers
    );
    assert!(
        !req.failure.sip_headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("via")),
        "structural headers are excluded"
    );

    // Call-scoped context attached by the framework.
    let snap = &req.snapshot;
    assert!(!snap.call_id.is_empty(), "a-leg Call-ID for correlation");
    assert_eq!(snap.callback_context.as_deref(), Some("ctx-snapshot"));
    assert_eq!(snap.legs.len(), 2, "a-leg + b-1: {:?}", snap.legs);
    assert_eq!(snap.legs[0].leg_id, "a");
    assert_eq!(snap.legs[1].leg_id, "b-1");
    assert!(snap.features.is_some(), "route features visible to the failure decision");
    // The observed trail: the backend derives `prov18x` (a Provisional ≥ 180
    // on the failed leg) instead of the platform exporting a bespoke flag.
    assert!(
        snap.cdr_events.iter().any(|e| e.event_type == CdrEventType::Provisional
            && e.leg_id == "b-1"
            && e.status_code == Some(180)),
        "Provisional trail present: {:?}",
        snap.cdr_events
    );
    assert!(
        snap.cdr_events
            .iter()
            .any(|e| e.event_type == CdrEventType::Reject && e.status_code == Some(486)),
        "the triggering reject is already on the trail"
    );

    let _ = h.finish().await;
}

// ── 2. b-leg INVITE transaction timeout → failover consult ──────────────────

#[tokio::test(start_paused = true)]
async fn b_leg_invite_transaction_timeout_consults_decision_and_reroutes() {
    // 1 ms transit: the reroute INVITE must be answered inside its 500 ms
    // Timer A window, and that window opens mid-`advance` — at the default
    // 100 ms per hop the answer loses the race and a retransmit crosses it.
    let h = Harness::with_transit_delay("timeout-failover", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // rings, then dead air
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-timeout".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                cap.lock().unwrap().push(req.clone());
                CallTreatment::Route(route_to("127.0.0.1", 5071))
            })
            .build(),
    );
    // SetupTimeout pushed past the sip-txn INVITE backstop (158 s) so the
    // transaction timeout is what fires; keepalive far out for a quiet tail.
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Dead gateway: carol never sends a final. At the sip-txn INVITE backstop
    // (158 s) the b-leg transaction times out — pre-fix this tore the call
    // down without ever consulting the decision backend. Advance just past the
    // deadline (not beyond it) so the reroute INVITE is answered inside its
    // Timer A window instead of retransmitting mid-advance.
    h.advance(Duration::from_secs(158) + Duration::from_millis(300)).await;

    // The reroute reaches bob, who answers promptly (before the b2bua's
    // Timer A re-sends the un-answered INVITE) …
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;

    // … and the failed leg is cleared: the b2bua CANCELs the still-early carol
    // leg. carol is the DEAD gateway (it never sent a final — that silence is
    // what timed the b-leg INVITE client txn out and triggered the reroute), so
    // it stays dead air and does not answer the CANCEL. This also keeps the trace
    // genuinely RFC-clean under unacked-invite-non-2xx-final: the b2bua's
    // carol INVITE client txn was already DELETED when the 158 s backstop fired
    // (sip-txn `fire_timeout` → `delete_txn`), so a late 487 here would land on
    // the unmatched-response path and never be hop-ACKed — an un-ACKable non-2xx
    // final. A truly dead gateway emits none, so there is nothing to ACK.
    let _cancel = carol.receive("CANCEL").await;
    let _ = &carol_uas; // its retained INVITE server txn sends no final (dead air)

    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 1, "one failure consult");
    assert_eq!(reqs[0].failure.origin, "transaction_timeout");
    assert_eq!(
        reqs[0].failure.timeout_kind.as_deref(),
        Some("transaction"),
        "carol rang, then went silent past the INVITE bound"
    );
    assert_eq!(reqs[0].failure.failed_leg_id.as_deref(), Some("b-1"));
    assert!(reqs[0].failure.sip_headers.is_empty(), "internal origin: no response headers");
    assert_eq!(reqs[0].snapshot.callback_context.as_deref(), Some("ctx-timeout"));
    drop(reqs);

    let _ = h.finish().await;
}

/// INVITE requests the recorded wire delivered to `to` — the original send
/// plus every Timer A rung.
fn invites_delivered_to(report: &scenario_harness::RunReport, to: SocketAddr) -> usize {
    report.entries().iter().filter(|e| e.to == to && e.raw.starts_with(b"INVITE ")).count()
}

/// The dead-hop case under a tightened first-response bound
/// (`invite_first_response_timeout_sec = 5`, ADR-0032 X1 amendment): carol
/// answers NOTHING — not even a `100` — so the b-leg gives up at 5 s, not at
/// Timer B, after exactly the three Timer A re-sends the bound buys
/// (0.5 / 1.5 / 3.5 s), and the `/call/failure` consult says
/// `timeout_kind: "response"` so the backend can route a dead hop apart from a
/// silent callee. The reroute lands on bob, the call is bridged, hung up, and
/// fully reaped.
#[tokio::test(start_paused = true)]
async fn b_leg_that_draws_nothing_fails_at_the_first_response_bound_as_response() {
    let h = Harness::with_transit_delay("first-response-bound-dead-hop", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // dead air from the start
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target
    let carol_addr: SocketAddr = "127.0.0.1:5070".parse().unwrap();

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-dead-hop".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                cap.lock().unwrap().push(req.clone());
                CallTreatment::Route(route_to("127.0.0.1", 5071))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.invite_first_response_timeout_sec = 5;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let carol_uas = carol.receive("INVITE").await;
    let _ = &carol_uas; // never responds: the hop is dead

    // The tightened bound fires at 5 s; advance just past it so the reroute
    // INVITE is answered inside its own Timer A window.
    h.advance(Duration::from_secs(5) + Duration::from_millis(300)).await;

    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    // The dead leg is cleared with a CANCEL carol never answers.
    let _cancel = carol.receive("CANCEL").await;

    {
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1, "one failure consult");
        assert_eq!(reqs[0].failure.origin, "transaction_timeout", "the origin is unchanged");
        assert_eq!(
            reqs[0].failure.timeout_kind.as_deref(),
            Some("response"),
            "nothing at all answered: the hop is dead"
        );
        assert_eq!(reqs[0].failure.failed_leg_id.as_deref(), Some("b-1"));
    }

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    // carol never answers her CANCEL, so the call rides the terminating
    // backstop out: advance exactly past it, then assert release.
    h.advance(Duration::from_millis(call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000)).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    assert_eq!(
        invites_delivered_to(&report, carol_addr),
        4,
        "original send + the three rungs a 5 s bound buys (0.5 / 1.5 / 3.5 s), none past it"
    );
}

/// A ringing callee is unaffected by the tightened first-response bound: under
/// the SAME 5 s bound carol's `180` at ~1 s swaps in the long INVITE bound
/// (`invite_txn_timeout_sec`, shortened to 40 s here), so the b-leg survives
/// the 5 s mark and gives up at 40 s with `timeout_kind: "transaction"` — it
/// answered, then went silent. Reroute, bridge, hang up, reap.
#[tokio::test(start_paused = true)]
async fn b_leg_that_rang_under_the_first_response_bound_fails_at_the_long_bound_as_transaction() {
    let h = Harness::with_transit_delay("first-response-bound-silent-callee", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // rings, then dead air
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-silent-callee".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                cap.lock().unwrap().push(req.clone());
                CallTreatment::Route(route_to("127.0.0.1", 5071))
            })
            .build(),
    );
    // The setup deadline stays strictly above the (shortened) INVITE bound so
    // the transaction timeout is what fires.
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.invite_first_response_timeout_sec = 5;
            c.invite_txn_timeout_sec = 40;
            c.setup_timeout_sec = 60;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    // Ring inside the first-response bound, after one Timer A rung.
    h.advance(Duration::from_secs(1)).await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Past the 5 s first-response bound: nothing fires, carol is ringing.
    h.advance(Duration::from_secs(6)).await;
    assert!(
        captured.lock().unwrap().is_empty(),
        "a ringing callee is not on the first-response bound"
    );
    assert_eq!(b2bua.active_calls(), 1);

    // The long bound is measured from the original send: it fires at 40 s.
    // Advance just past it so the reroute INVITE is answered inside its own
    // Timer A window.
    h.advance(Duration::from_secs(33) + Duration::from_millis(300)).await;

    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    // The silent leg is cleared with a CANCEL carol never answers.
    let _cancel = carol.receive("CANCEL").await;

    {
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1, "one failure consult");
        assert_eq!(reqs[0].failure.origin, "transaction_timeout", "the origin is unchanged");
        assert_eq!(
            reqs[0].failure.timeout_kind.as_deref(),
            Some("transaction"),
            "carol answered a provisional, then went silent past the INVITE bound"
        );
        assert_eq!(reqs[0].failure.failed_leg_id.as_deref(), Some("b-1"));
    }

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    // carol never answers her CANCEL, so the call rides the terminating
    // backstop out: advance exactly past it, then assert release.
    h.advance(Duration::from_millis(call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000)).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _ = h.finish().await;
}

// ── 3. Failover Route parity: limiter admitted + released ───────────────────

#[tokio::test]
async fn failover_route_limiter_is_admitted_and_released_at_termination() {
    let h = Harness::with_transit_delay("failover-limiter-parity", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // first target — 503s
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target

    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-parity".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                // The reroute carries its own per-target limit + service ext —
                // dropped silently before failover/initial parity landed.
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-B".into(), limit: 5 }];
                r.service_ext =
                    [("svc-x".to_string(), serde_json::json!({"k": "v"}))].into_iter().collect();
                CallTreatment::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    carol.receive("INVITE").await.respond(503, "Service Unavailable").await;
    carol.receive("ACK").await;

    // The failover route's limiter entry is INCRed against the new target.
    let mut bob_uas = bob.receive("INVITE").await;
    settle_until(|| store.stats().current_total == 1).await;
    assert_eq!(store.stats().current_total, 1, "failover route admitted its limiter entry");

    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Termination releases the failover-admitted hold — proving it was
    // recorded ON the call (not leaked in the fold).
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "BYE released the failover hold");

    let _ = h.finish().await;
}

// ── 3b. Failover Route limiter reject → bounded re-consult ──────────────────

#[tokio::test]
async fn failover_route_limiter_reject_reconsults_with_call_limiter_origin() {
    let h = Harness::with_transit_delay("failover-limiter-reject", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // first target — 503s
    let bob = h.agent("bob", "127.0.0.1:5071").await; // final target

    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-chain".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                cap.lock().unwrap().push(req.clone());
                if req.failure.origin == "call_limiter" {
                    // Second hop: the capped target was rejected → route to
                    // bob, no limiter.
                    return CallTreatment::Route(route_to("127.0.0.1", 5071));
                }
                // First hop: a full trunk (limit 0 rejects immediately). The
                // callback context is what allows the chained re-consult.
                let mut r = route_to("127.0.0.1", 5099);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-full".into(), limit: 0 }];
                r.callback_context = Some("ctx-chain-2".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    carol.receive("INVITE").await.respond(503, "Service Unavailable").await;
    carol.receive("ACK").await;

    // The full trunk never receives an INVITE; the chain lands on bob.
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;

    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 2, "external failure, then the limiter-reject re-consult");
    assert_eq!(reqs[0].failure.origin, "external");
    assert_eq!(reqs[1].failure.origin, "call_limiter");
    assert_eq!(reqs[1].failure.limiter_id.as_deref(), Some("trunk-full"));
    assert_eq!(reqs[1].callback_context.as_deref(), Some("ctx-chain-2"));
    drop(reqs);
    assert_eq!(store.stats().current_total, 0, "the rejected trunk holds nothing");

    let _ = h.finish().await;
}
