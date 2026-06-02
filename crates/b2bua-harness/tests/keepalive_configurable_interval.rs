//! The in-dialog OPTIONS keepalive interval is operator-configurable via
//! `B2buaConfig::keepalive_interval_sec` (production default 300 s, env
//! `B2BUA_KEEPALIVE_SEC`). This pins the wiring: a call on a worker configured
//! with interval N gets its keepalive timer armed at N — not the harness's 30 s
//! baseline. We pick N = 90 s (distinct from both the 30 s harness baseline and
//! the 300 s production default), advance to just under N (no OPTIONS yet), then
//! to N (OPTIONS fires to both legs). A second cycle proves the re-arm also uses
//! the configured value.
//!
//! Faithful to the CLAUDE.md test-clock hazards: every advance lands on exactly
//! one deadline; we drive the OPTIONS round-trip between advances.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::B2buaSut;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A bespoke keepalive interval, distinct from the 30 s harness baseline and the
/// 300 s production default, so the assertion can only pass if the configured
/// value actually drives the timer.
const CONFIGURED_INTERVAL_SEC: i64 = 90;

#[tokio::test(start_paused = true)]
async fn keepalive_interval_honors_config() {
    let h = Harness::new("b2bua-keepalive-configurable");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5076));
    let b2bua = B2buaSut::start_with_config(
        &h,
        "b2bua",
        "127.0.0.1:5086",
        decision,
        None,
        |c| c.keepalive_interval_sec = CONFIGURED_INTERVAL_SEC,
    )
    .await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── Just before the configured interval: no keepalive yet ────────────────
    // Advance to one tick short of the deadline (the 30 s harness baseline would
    // already have fired here; the configured 90 s must not have).
    h.advance(Duration::from_secs((CONFIGURED_INTERVAL_SEC - 1) as u64)).await;

    // ── At the configured interval: OPTIONS to both legs ─────────────────────
    h.advance(Duration::from_secs(1)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Second cycle — confirms the re-arm also uses the configured value ─────
    h.advance(Duration::from_secs(CONFIGURED_INTERVAL_SEC as u64)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Teardown ─────────────────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
