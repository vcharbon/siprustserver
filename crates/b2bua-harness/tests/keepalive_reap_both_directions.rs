//! Keepalive-driven reaping of a dead peer, asserted VIA CDR GENERATION, for
//! BOTH directions — and at scale across multiple concurrent calls.
//!
//! Regression for the production "no-BYE orphan" leak: an ESTABLISHED dialog
//! whose peer vanished with no BYE was never reaped, so `b2bua_active_calls`
//! grew without bound. The intended cleanup is the in-dialog OPTIONS keepalive
//! (`keepalive` rule) → unanswered OPTIONS → `KeepaliveTimeout` →
//! `keepalive-timeout` rule → `BeginTermination` → reap.
//!
//! Root cause it guards: the single shared `TimerService` keyed its live-epoch
//! map by the bare timer **id** (`"Keepalive"`, `"KeepaliveTimeout:a"`, …) which
//! is identical across calls. Scheduling a later call's keepalive overwrote an
//! earlier call's epoch, tombstoning the earlier keepalive so it never fired —
//! at scale, keepalives stopped and dead peers were never probed. The fix keys
//! the map by `(call_ref, id)`. `colliding_timer_ids_across_calls_both_fire`
//! covers the driver; these tests cover the end-to-end reap + CDR.
//!
//! Mirrors `keepalive_timeout.rs`: a 30 s keepalive interval + a hard 5 s
//! per-leg keepalive timeout. We assert teardown via `b2bua.cdr_records()`
//! (a CDR is written and contains a `Bye`/teardown event) and via paired
//! creations/removals metrics.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Caller (A leg) goes silent on its keepalive OPTIONS → the call is torn down
/// and a CDR with a teardown (Bye) event is written.
#[tokio::test(start_paused = true)]
async fn caller_silent_keepalive_reaps_with_cdr() {
    let h = Harness::new("b2bua-keepalive-reap-a-silent");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5085", "127.0.0.1", 5075).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // Keepalive probe: alice (A) stays silent, bob (B) answers.
    h.advance(KEEPALIVE_INTERVAL).await;
    let _silent = alice.receive("OPTIONS").await; // received, never answered
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // A-leg keepalive times out → terminate A, BYE the healthy peer (bob).
    h.advance(KEEPALIVE_TIMEOUT).await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the reaped call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(
        kinds.contains(&CdrEventType::Bye),
        "CDR records the keepalive-timeout teardown (Bye): {kinds:?}",
    );

    let _report = h.finish().await;
}

/// Callee (B leg) goes silent on its keepalive OPTIONS → the call is torn down
/// and a CDR with a teardown (Bye) event is written. Symmetric to the A case;
/// here the BYE goes to the healthy caller (alice).
#[tokio::test(start_paused = true)]
async fn callee_silent_keepalive_reaps_with_cdr() {
    let h = Harness::new("b2bua-keepalive-reap-b-silent");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5086", "127.0.0.1", 5076).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // Keepalive probe: bob (B) stays silent, alice (A) answers.
    h.advance(KEEPALIVE_INTERVAL).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    let _silent = bob.receive("OPTIONS").await; // received, never answered

    // B-leg keepalive times out → terminate B, BYE the healthy peer (alice).
    h.advance(KEEPALIVE_TIMEOUT).await;
    alice.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the reaped call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(
        kinds.contains(&CdrEventType::Bye),
        "CDR records the keepalive-timeout teardown (Bye): {kinds:?}",
    );

    let _report = h.finish().await;
}

/// At-scale regression: TWO concurrent established calls on ONE B2BUA (one
/// shared TimerService). The earlier call's keepalive must still fire after a
/// later call arms its own identically-named timer. Both callers go silent;
/// both calls must reap. Under the cross-call aliasing bug the first call's
/// keepalive was tombstoned and it leaked (creations 2, removals 1).
#[tokio::test(start_paused = true)]
async fn two_calls_both_reap_despite_shared_timer_ids() {
    // 1 ms transit so the OPTIONS / BYE 2xx round-trips settle inside Timer E
    // (500 ms) before a retransmit (see keepalive_via_proxy.rs).
    let h = Harness::with_transit_delay("b2bua-keepalive-reap-two-calls", 1);
    let alice1 = h.agent("alice1", "127.0.0.1:5067").await;
    let alice2 = h.agent("alice2", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5087", "127.0.0.1", 5077).await;

    // ── Call 1 setup ─────────────────────────────────────────────────────────
    let mut c1 = alice1.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut u1 = bob.receive("INVITE").await;
    u1.respond(180, "Ringing").await;
    c1.expect(180).await;
    u1.respond(200, "OK").with_sdp(ANSWER).await;
    c1.expect(200).await;
    let _d1 = c1.ack().await;
    bob.receive("ACK").await;

    // ── Call 2 setup (arms a SECOND "Keepalive" timer — the aliasing trigger) ─
    let mut c2 = alice2.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut u2 = bob.receive("INVITE").await;
    u2.respond(180, "Ringing").await;
    c2.expect(180).await;
    u2.respond(200, "OK").with_sdp(ANSWER).await;
    c2.expect(200).await;
    let _d2 = c2.ack().await;
    bob.receive("ACK").await;

    assert_eq!(b2bua.metrics().creations_total(), 2, "two calls created");

    // ── Keepalive probe: both callers silent, bob answers both legs ──────────
    h.advance(KEEPALIVE_INTERVAL).await;
    // Both A-legs receive OPTIONS but never answer (the dead-caller shape).
    let _s1 = alice1.receive("OPTIONS").await;
    let _s2 = alice2.receive("OPTIONS").await;
    // Bob's two B-legs answer (one OPTIONS per call). Tolerate retransmits of
    // the still-unanswered probe while we reach the next one.
    bob.receive_tolerating("OPTIONS", &["OPTIONS"]).await.respond(200, "OK").await;
    bob.receive_tolerating("OPTIONS", &["OPTIONS"]).await.respond(200, "OK").await;

    // ── Both A-leg keepalives time out → both calls reap (BYE to bob) ────────
    // Drain bob's inbound until both BYEs are answered, 200-ing any retransmit.
    h.advance(KEEPALIVE_TIMEOUT).await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bob.receive_tolerating("BYE", &["OPTIONS", "BYE"]).await.respond(200, "OK").await;

    settle_until(|| b2bua.cdr_records().len() >= 2).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 2, "BOTH calls produce a CDR (neither leaked)");
    for cdr in &cdrs {
        let kinds: Vec<CdrEventType> = cdr.events.iter().map(|e| e.event_type).collect();
        assert!(kinds.contains(&CdrEventType::Bye), "each CDR has a Bye event: {kinds:?}");
    }
    // No orphaned established call leaked.
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
