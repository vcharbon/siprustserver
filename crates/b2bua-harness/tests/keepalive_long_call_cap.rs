//! A long-lived healthy call must survive its own liveness machinery.
//!
//! `max_messages_per_call` is a cap-DEFENSE against a runaway dialog (a peer
//! that never stops, a glare loop, an OPTIONS storm). The stack's own keepalive
//! probes are self-paced — one round per configured interval — so they are
//! never evidence of a runaway peer; a cap they consume turns the defense on
//! exactly the calls the liveness machinery exists to keep alive (an hour-long
//! call self-destructs around message 200 with
//! `BYE Reason: SIP;cause=503;text="message-cap-exceeded"`).
//!
//! The contract pinned here: the cap budget is a PER-LIVENESS-INTERVAL rate —
//! each keepalive tick opens a fresh window — so a call outlasting any number
//! of intervals never trips it, while `>cap` events INSIDE one window (the
//! storm tests in `limit_cases.rs`) still do.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::B2buaSut;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const INTERVAL_SEC: i64 = 120;

/// Keepalive rounds to run — the whole span stays under the route's 3 600 s
/// `GlobalDuration` so the duration cap never competes with the message cap.
const ROUNDS: usize = 28;

/// A cap the accumulated liveness traffic EXCEEDS across the call while no
/// single interval comes near it: each round costs the rule chain about three
/// in-dialog events (the tick and both OPTIONS `200`s), so 28 rounds ≈ 84
/// lifetime events against ≤ 3 per interval. A lifetime reading trips mid-call;
/// the per-interval rate contract never does.
const CAP: u64 = 60;

#[tokio::test(start_paused = true)]
async fn a_long_call_with_keepalives_never_trips_the_message_cap() {
    let h = Harness::new("b2bua-keepalive-long-call-cap");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5077));
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = INTERVAL_SEC;
            c.max_messages_per_call = CAP;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5087")
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

    // ── The long middle: ROUNDS healthy keepalive cycles ─────────────────────
    for round in 0..ROUNDS {
        h.advance(Duration::from_secs(INTERVAL_SEC as u64)).await;
        alice.receive("OPTIONS").await.respond(200, "OK").await;
        bob.receive("OPTIONS").await.respond(200, "OK").await;
        assert_eq!(
            b2bua.metrics().message_cap_terminated_total(),
            0,
            "the stack's own liveness probes consumed the cap budget (round {round})"
        );
    }

    // ── The call is still alive: a normal caller BYE ends it ─────────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
