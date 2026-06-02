//! Regression: a BYE that never receives its 200 OK must still be reaped.
//!
//! Under packet loss / chaos (a lost BYE, a dead UAS, or proxy churn mid-
//! teardown) the peer leg sits at `ByeSent` — a *non-terminal* disposition — so
//! `is_fully_resolved` never passes, the call wedges in `Terminating` forever,
//! `RemoveCall` is never emitted, and `b2bua_active_calls` never decrements. In
//! k8s this leaked ~8700 dead dialogs (active_calls pinned flat for HOURS after
//! all traffic stopped), growing worker memory without bound until the load
//! generators OOM'd.
//!
//! The 32 s `TerminatingTimeout` safety timer (armed by `begin_termination`)
//! must force-resolve the wedged leg and reap the call. This guards the
//! `terminating-safety-timeout` rule, which used to be a no-op.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// `TERMINATING_TIMEOUT_MS` in `crates/call/src/helpers.rs`.
const TERMINATING_TIMEOUT: Duration = Duration::from_secs(32);

#[tokio::test(start_paused = true)]
async fn unanswered_bye_is_reaped_by_safety_timer() {
    let h = Harness::new("b2bua-bye-no-200-reap");
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5088", "127.0.0.1", 5078).await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "exactly one active call after setup",
    );

    // ── alice hangs up; the B2BUA relays BYE to bob — bob NEVER answers ───────
    // Receiving the relayed BYE proves the B2BUA processed alice's BYE and is now
    // in Terminating with bob's leg wedged at ByeSent. We deliberately do not
    // respond, modelling a lost 200 / dead peer.
    let _bye = dialog.bye().await;
    bob.receive("BYE").await; // received, intentionally unanswered

    // ── Safety net: 32 s later the wedged call must be reaped ─────────────────
    h.advance(TERMINATING_TIMEOUT + Duration::from_secs(1)).await;
    for _ in 0..50 {
        if b2bua.metrics().removals_total() == b2bua.metrics().creations_total() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let m = b2bua.metrics();
    assert_eq!(
        m.removals_total(),
        m.creations_total(),
        "BYE-without-200 must be reaped by the 32s safety timer (active_calls -> 0); \
         got creations={} removals={}",
        m.creations_total(),
        m.removals_total(),
    );

    let _report = h.finish().await;
}
