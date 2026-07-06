//! The subscribed release-event decision seam + the established-call reroute
//! treatment (newkahneed-009):
//!
//!   1. UNSUBSCRIBED max-duration expiry → today's local teardown, and the
//!      engine's `call_release` is NEVER consulted (even with a
//!      callback_context — the subscription is the gate).
//!   2. Subscribed + engine says `Release` → same local teardown, and the
//!      consult carried the `CallSnapshot`, the callback_context, and the
//!      `max_call_duration` event kind.
//!   3. Subscribed + engine says `Route` → the CONNECTED call is rerouted:
//!      replacement b-leg dialed (A's offer), answered, ACKed; the a-leg is
//!      re-INVITEd onto the new answer SDP; the old b-leg is BYEd; the call
//!      continues and a normal hangup works. Limiter parity: the reroute
//!      route's `call_limiter` holds are admitted and released.
//!   4. The reroute route OWNS the follow-up policy: its features re-arm the
//!      GlobalDuration cap and its (empty) `subscriptions` replace the
//!      original registry, so the SECOND expiry tears down locally with no
//!      second consult.
//!   5. A hung / erroring `call_release` falls back to local teardown,
//!      bounded by the decision deadline — no wedged call.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallLimiterEntry, CallReleaseRequest, CallReleaseResponse, NewCallResponse, ReleaseOutcome,
    ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{settle_until, B2buaSut};
use call::ReleaseEventKind;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
// The announcement / MRF target's answer (the SDP the a-leg must be realigned to).
const MRF_ANSWER: &str = "v=0\r\no=mrf 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

async fn serve_limiter(net: &SimulatedHttpNetwork) -> (Arc<WindowStore>, Box<dyn HttpServerHandle>) {
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (store, handle)
}

fn limiter_client(net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    // A 1 s fail-open budget (vs the 150 ms other suites use): the reroute
    // admit runs inside the release fire-and-forget task, which is woken by a
    // timer INSIDE a big `h.advance` — the harness advances in 100 ms chunks,
    // so a 150 ms budget can expire between chunks before the simulated HTTP
    // round-trip is delivered (a paused-clock pumping artifact, not SUT).
    Arc::new(HttpCallLimiter::new(
        Arc::new(net.clone()),
        laddr(),
        Duration::from_secs(1),
    ))
}

/// Re-answer a Timer-A retransmit of the replacement-leg INVITE as a real UAS
/// would — the SAME 200 (same To-tag, same SDP), a faithful RFC 3261 §17.2.1
/// 2xx retransmission. The release fold pipeline (timer fire → consult →
/// re-entry → CreateLeg) spans several 100 ms pump chunks under the paused
/// clock, so the b-leg INVITE's first 500 ms retransmit usually beats the
/// test's 200; answering it with a FRESH tag (what `receive_tolerating`'s
/// auto-200 does) would fabricate a phantom fork dialog and trip the
/// `rfc3261.unackedInvite2xxByed` audit. Non-blocking: if no retransmit is
/// queued (timing shifted), this is a no-op — the ACK cannot be queued yet
/// because it is only sent after the 200 above is processed.
async fn absorb_invite_retransmit(
    mrf: &scenario_harness::agent::Agent,
    answered: &scenario_harness::agent::ServerTxn,
    sdp: &str,
) {
    let tag = answered.dialog().local_tag().to_string();
    while let Some(mut retrans) = mrf.try_receive_tolerating("INVITE", &[]).await {
        retrans.respond(200, "OK").with_sdp(sdp).with_to_tag(&tag).await;
    }
}

/// Route to `bob_port` with a 60 s cap; `subscribe` toggles the
/// max-call-duration release subscription (the callback_context is ALWAYS set,
/// so test 1 proves the subscription — not the context — is the gate).
fn route_with_cap(bob_port: u16, subscribe: bool) -> b2bua::decision::RouteDecision {
    let mut r = route_to("127.0.0.1", bob_port);
    r.features.platform.max_duration_sec = 60;
    r.callback_context = Some("ctx-release".into());
    if subscribe {
        r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
    }
    r
}

// ── 1. unsubscribed → local teardown, engine NOT consulted ─────────────────

#[tokio::test(start_paused = true)]
async fn unsubscribed_max_duration_keeps_local_teardown_without_consult() {
    let h = Harness::new("release-unsubscribed-local");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let consults = Arc::new(AtomicUsize::new(0));
    let c = consults.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| NewCallResponse::Route(route_with_cap(5070, false)))
            .on_release(move |_req| {
                c.fetch_add(1, Ordering::SeqCst);
                ReleaseOutcome::Respond(CallReleaseResponse::Release)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(61)).await;

    // Local teardown as today: both legs BYEd, max_duration CDR, no consult.
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    assert_eq!(consults.load(Ordering::SeqCst), 0, "unsubscribed expiry must NOT consult call_release");
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert!(
        b2bua.cdr_records()[0]
            .events
            .iter()
            .any(|e| e.reason.as_deref() == Some("max_duration")),
        "CDR carries the max_duration reason",
    );

    let _ = h.finish().await;
}

// ── 2. subscribed + Release → teardown, consult carried snapshot + event ────

#[tokio::test(start_paused = true)]
async fn subscribed_release_consults_engine_then_tears_down() {
    let h = Harness::new("release-subscribed-release");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;

    let captured: Arc<Mutex<Vec<CallReleaseRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| NewCallResponse::Route(route_with_cap(5071, true)))
            .on_release(move |req| {
                cap.lock().unwrap().push(req.clone());
                ReleaseOutcome::Respond(CallReleaseResponse::Release)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(61)).await;

    // Engine said Release → the same local teardown as the unsubscribed path.
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one release consult");
    let req = &reqs[0];
    assert_eq!(req.event, ReleaseEventKind::MaxCallDuration, "correct event kind");
    assert_eq!(req.callback_context.as_deref(), Some("ctx-release"));
    // The framework-attached CallSnapshot (002 parity): both legs + the
    // observed CDR trail with the answer are visible to the backend.
    assert_eq!(req.snapshot.legs.len(), 2, "a-leg + b-leg in the snapshot");
    assert_eq!(req.snapshot.legs[0].leg_id, "a");
    assert!(
        req.snapshot
            .cdr_events
            .iter()
            .any(|e| matches!(e.event_type, call::CdrEventType::Answer)),
        "snapshot CDR trail carries the answer",
    );
    drop(reqs);

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert!(
        b2bua.cdr_records()[0]
            .events
            .iter()
            .any(|e| e.reason.as_deref() == Some("max_duration")),
        "CDR carries the max_duration reason",
    );

    let _ = h.finish().await;
}

// ── 3. subscribed + Route → established-call reroute, then normal hangup ───

#[tokio::test(start_paused = true)]
async fn subscribed_route_reroutes_established_call_then_normal_hangup() {
    let h = Harness::new("release-reroute-happy");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let mrf = h.agent("mrf", "127.0.0.1:5092").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_with_cap(5072, true);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-A".into(), limit: 10 }];
                NewCallResponse::Route(r)
            })
            .on_release(move |_req| {
                // Reroute the released call to the announcement target, with
                // its own limiter hold (output parity: admitted + released).
                let mut r = route_to("127.0.0.1", 5092);
                r.call_limiter =
                    vec![CallLimiterEntry { id: "announce-cap".into(), limit: 10 }];
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5082")
        .await;

    // ── establish A↔B ───────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(store.stats().current_total, 1, "established call holds the trunk-A slot");

    // ── the cap expires; the engine converts release → reroute ─────────────
    h.advance(Duration::from_secs(61)).await;

    // Replacement b-leg toward the MRF carries A's original offer.
    let mut mrf_uas = mrf.receive("INVITE").await;
    assert_eq!(
        String::from_utf8_lossy(&mrf_uas.request().body),
        OFFER,
        "replacement INVITE carries A's INVITE-snapshot offer",
    );
    mrf_uas.respond(200, "OK").with_sdp(MRF_ANSWER).await;
    absorb_invite_retransmit(&mrf, &mrf_uas, MRF_ANSWER).await;
    mrf.receive("ACK").await;

    // The a-leg is re-INVITEd onto the MRF's answer SDP (the bridge).
    let mut a_realign = alice.receive("INVITE").await;
    assert_eq!(
        String::from_utf8_lossy(&a_realign.request().body),
        MRF_ANSWER,
        "a-leg realign re-INVITE offers the replacement leg's answer SDP",
    );
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;

    // The displaced original b-leg is BYEd.
    bob.receive("BYE").await.respond(200, "OK").await;

    // Limiter parity: the reroute's hold was admitted alongside the original.
    settle_until(|| store.stats().current_total == 2).await;
    assert_eq!(store.stats().current_total, 2, "trunk-A + announce-cap holds live");

    // ── the rerouted call continues; a normal hangup works ────────────────
    h.advance(Duration::from_secs(5)).await;
    let mut alice_bye = alice_dialog.bye().await;
    mrf.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "both holds released at hangup");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert!(
        cdrs[0]
            .events
            .iter()
            .any(|e| e.reason.as_deref() == Some("release-reroute-completed")),
        "CDR records the completed reroute: {:?}",
        cdrs[0].events,
    );

    let _ = h.finish().await;
}

// ── 4. the reroute route owns the follow-up policy (features + subscriptions) ─

#[tokio::test(start_paused = true)]
async fn reroute_route_rearms_cap_and_owns_subscriptions() {
    let h = Harness::new("release-reroute-rearm");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let mrf = h.agent("mrf", "127.0.0.1:5093").await;

    let consults = Arc::new(AtomicUsize::new(0));
    let c = consults.clone();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| NewCallResponse::Route(route_with_cap(5073, true)))
            .on_release(move |_req| {
                c.fetch_add(1, Ordering::SeqCst);
                // The reroute route re-arms a fresh 60 s cap and (deliberately)
                // declares NO subscriptions — it owns the registry, so the
                // second expiry must act locally with no second consult.
                let mut r = route_to("127.0.0.1", 5093);
                r.features.platform.max_duration_sec = 60;
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5083")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // First expiry → reroute to the MRF.
    h.advance(Duration::from_secs(61)).await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_ANSWER).await;
    absorb_invite_retransmit(&mrf, &mrf_uas, MRF_ANSWER).await;
    mrf.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    assert_eq!(consults.load(Ordering::SeqCst), 1, "first expiry consulted once");

    // Second expiry (the reroute route's re-armed 60 s cap — SetFeatures +
    // GlobalDuration parity) → LOCAL teardown, because the reroute route's
    // empty `subscriptions` replaced the original registry.
    h.advance(Duration::from_secs(61)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    mrf.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(
        consults.load(Ordering::SeqCst),
        1,
        "second expiry must NOT consult (subscriptions were replaced by the reroute route)",
    );

    let _ = h.finish().await;
}

// ── 5. hung / erroring consult → bounded fallback to local teardown ────────

#[tokio::test(start_paused = true)]
async fn hung_release_consult_falls_back_to_local_teardown_within_deadline() {
    let h = Harness::new("release-consult-hang");
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| NewCallResponse::Route(route_with_cap(5074, true)))
            .on_release(move |_req| ReleaseOutcome::Hang)
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5084")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // Expiry fires; the consult hangs. The DeadlineDecisionEngine bound
    // (call_control_timeout_ms, default 5000) resolves it → local teardown.
    h.advance(Duration::from_secs(61)).await;
    h.advance(Duration::from_secs(6)).await;

    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert!(
        b2bua.cdr_records()[0]
            .events
            .iter()
            .any(|e| e.reason.as_deref() == Some("max_duration")),
        "fallback teardown still writes the max_duration CDR",
    );

    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn erroring_release_consult_falls_back_to_local_teardown() {
    let h = Harness::new("release-consult-error");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| NewCallResponse::Route(route_with_cap(5075, true)))
            .on_release(move |_req| ReleaseOutcome::Error)
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5085")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(61)).await;

    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _ = h.finish().await;
}
