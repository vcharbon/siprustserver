//! **Silent-callee no-answer teardown through a REAL front proxy (via-LB).**
//!
//! The downstream via-LB siblings of `cancel_200_crossing_internal.rs`: the same
//! internally-originated CANCEL / crossing-200 reap, but exercised across a real
//! `sip_proxy::ProxyCore` sitting between the caller and the worker AND between
//! the worker and the callee (the production b-leg-through-the-LB topology,
//! `B2BUA_OUTBOUND_PROXY`). This is the shape the `f14bb06` call-liveness fix was
//! flagged as "largely subsuming":
//!
//!   alice :5060 ─▶ proxy :5080 ─▶ b1 :5091 ─▶ proxy :5080 ─▶ bob :5070
//!                  (real LoadBalancer)   (B2buaCore; b-leg via the proxy)
//!
//! The callee is **silent** — it never sends a `>=180`. Its bare `100 Trying` is
//! absorbed by the proxy (RFC 3261 §16.7 — a proxy MUST NOT forward a 100), so
//! the worker NEVER sees a provisional and the b-leg INVITE keeps retransmitting
//! hop-by-hop (Timer A). Teardown is therefore driven **only** by the worker's
//! OWN per-call `TimerType::NoAnswer` (armed at leg creation in
//! `actions.rs::CreateLeg`, independent of any 18x), NOT by an 18x. When it
//! fires the worker CANCELs the ringing b-leg; the callee's `200 OK` can then
//! cross the CANCEL on the wire.
//!
//! What `f14bb06` bought here: a leg in the `Cancelling` disposition is NOT
//! resolved (`call::helpers::leg_is_resolved`), so finalization HOLDS the call
//! alive until the internal CANCEL settles — a `487`, or the crossing `200`
//! reaped by `cancel-200-crossing` (ACK + BYE to the late callee). The reap is
//! therefore **timing-independent**: it wins whether the `200` lands right after
//! the CANCEL or many seconds later, and it does so behind a retransmitting
//! proxy that the worker never heard a provisional through. These tests pin that
//! at the SUT level through the genuine proxy (the harness the downstream
//! `bc_02_bl_*__via_lb` cases live over).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use failover_harness::FailoverHarness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const CHARLIE: &str = "127.0.0.1:5071"; // reroute target (failover variant)
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";

// A short no-answer deadline keeps the paused-clock advance (and the b-leg's
// Timer A retransmit churn) small; it fires FAR below the 150 s setup timeout and
// the ~158 s sip-txn INVITE backstop, so `NoAnswer` is unambiguously what tears
// the ringing b-leg down.
const NO_ANSWER_SEC: i64 = 5;

/// Decision that routes every call to `dest_port`, arming the per-call
/// `NoAnswer` deadline. `callback_context = None` → the `no-answer` rule takes
/// its **reject** branch (`BeginTermination`, no `/call/failure` consult).
fn reject_decision(dest_port: u16) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", dest_port);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Wait for the single worker to fully reap the call: `creations == removals`
/// (call removed) and per-call memory clean (no live call, no lingering lock).
async fn assert_reaped(fh: &FailoverHarness, b1: &failover_harness::ReplicatedB2buaSut) {
    let reaped = fh
        .settle_terminal(|| async {
            b1.metrics().removals_total() == b1.metrics().creations_total() && b1.memory_clean()
        })
        .await;
    assert!(
        reaped,
        "the silent-callee no-answer call must be fully reaped (creations {} != removals {}, or \
         memory not clean: {} live / {} locks)",
        b1.metrics().creations_total(),
        b1.metrics().removals_total(),
        b1.active_calls(),
        b1.lock_count(),
    );
}

/// **Reject path, prompt crossing.** The worker's NoAnswer timer fires, the b-leg
/// is CANCELed through the proxy, and the silent callee answers `200` crossing
/// the CANCEL. The abandoned callee MUST be reaped (ACK + BYE via the proxy) and
/// the caller MUST still get its final reject — behind the retransmitting LB.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn silent_callee_no_answer_via_lb__reject__reaps_crossing_200() {
    let mut fh = FailoverHarness::new("s10b-silent-callee-noanswer-reject", &["b1"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &[],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            reject_decision(5070),
            Arc::new(NoopLimiter),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready(), "the lone worker is ready at steady state");

    // ── alice → proxy → b1 → proxy → bob ─────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;

    // The SILENT callee: it accepts the b-leg INVITE but sends NO >=180. Its bare
    // 100 Trying is absorbed by the proxy, so the worker never sees a provisional.
    let mut bob_uas = bob.receive("INVITE").await;

    // ── Trip the worker's own NoAnswer timer → CANCEL the ringing b-leg ──────
    fh.advance(Duration::from_secs(NO_ANSWER_SEC as u64) + Duration::from_millis(300)).await;

    // The CANCEL egresses through the proxy (cancel_lru matches the remembered
    // b-leg INVITE → forwards to bob). The call MUST outlive the CANCEL.
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    cancel.respond(200, "OK").await;

    // ── CROSSING: bob answers 200 OK, crossing the CANCEL on the wire ────────
    // The abandoned callee MUST be reaped — ACK then immediate BYE via the proxy.
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    let mut bye = bob.receive_absorbing("BYE", &["INVITE"]).await;
    bye.respond(200, "OK").await;

    // The caller still gets its final reject (ADR-0022 unanswered-a-leg synthesis,
    // once the abandoned callee has quiesced) — NOT dropped on a removed call.
    let failed = call.expect(503).await;
    assert_eq!(failed.status, 503, "caller's INVITE resolves with a final failure");
    // Flush alice's §17.1.1.3 ACK for the 503 (sent by the receive above) the
    // one hop to the proxy before the Drop-time RFC gate snapshots the trace.
    fh.advance(Duration::from_millis(10)).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **Reject path, DELAYED crossing — the timing-independence proof.** Identical to
/// the prompt case, but the crossing `200` is delivered many seconds AFTER the
/// CANCEL (well within the ~32 s terminating-safety backstop). Pre-`f14bb06` the
/// reject teardown promoted `Terminating → Terminated → RemoveCall` in the SAME
/// turn as the CANCEL, so ANY later `200` landed on a removed call (no ACK, no
/// BYE — the callee orphaned). Holding a `Cancelling` leg unresolved keeps the
/// call alive across the gap, so the reap holds "regardless of WHEN" the `200`
/// arrives — the whole point of the call-liveness fix, proven through the real LB.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn silent_callee_no_answer_via_lb__reject__delayed_crossing_200_still_reaped() {
    let mut fh = FailoverHarness::new("s10b-silent-callee-noanswer-reject-delayed", &["b1"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &[],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            reject_decision(5070),
            Arc::new(NoopLimiter),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;

    fh.advance(Duration::from_secs(NO_ANSWER_SEC as u64) + Duration::from_millis(300)).await;
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    cancel.respond(200, "OK").await;

    // ── The call is now HELD Terminating (b-leg `Cancelling`, awaiting its 487
    // or a crossing 200). Advance well past the CANCEL — the abandoned callee's
    // answer is genuinely late — but stay inside the terminating-safety window. ─
    fh.advance(Duration::from_secs(10)).await;

    // ── CROSSING (late): bob finally answers 200 → still reaped ──────────────
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    let mut bye = bob.receive_absorbing("BYE", &["INVITE"]).await;
    bye.respond(200, "OK").await;

    let failed = call.expect(503).await;
    assert_eq!(failed.status, 503, "caller's INVITE resolves with a final failure after a late reap");
    // Flush alice's §17.1.1.3 ACK for the 503 the one hop to the proxy before
    // the Drop-time RFC gate snapshots the trace.
    fh.advance(Duration::from_millis(10)).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **Failover / reroute path, crossing.** The via-LB sibling of
/// `no_answer_cancel_crossed_by_200_reaps_the_abandoned_callee_and_failover_proceeds`
/// (which is direct-topology). A `callback_context` keeps the call Active while
/// `/call/failure` is consulted at the NoAnswer timeout; the crossing `200` on the
/// abandoned callee is reaped (ACK + BYE via the proxy) AND the reroute to the
/// second target proceeds — the caller is bridged. Mirrors the downstream
/// `bc_02_bl_reroutes_on_18x__via_lb` intent, silent-callee flavour.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn silent_callee_no_answer_via_lb__reroute__reaps_crossing_200_and_reroutes() {
    let mut fh = FailoverHarness::new("s10b-silent-callee-noanswer-reroute", &["b1"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await; // rings silent, no-answer'd, answers late
    let charlie = fh.agent("charlie", CHARLIE).await; // reroute target

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;

    // Failover-capable decision: NoAnswer + a callback_context (call stays Active
    // for the /call/failure consult), rerouting on failure to charlie (5071).
    let decision: Arc<dyn CallDecisionEngine> = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                r.callback_context = Some("silent-callee-reroute-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                // Behind the LB the b-leg is forwarded to its Request-URI, and a
                // reroute leg's R-URI defaults to the a-leg's (bob). Name the real
                // reroute target explicitly so the proxy forwards to charlie, not
                // back to bob (the `test_adapter` anti-loop invariant).
                let mut r = route_to("127.0.0.1", 5071);
                r.new_ruri = Some("sip:127.0.0.1:5071".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    let b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &[],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            decision,
            Arc::new(NoopLimiter),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;

    // NoAnswer fires → CANCEL bob + /call/failure consult (call stays Active).
    fh.advance(Duration::from_secs(NO_ANSWER_SEC as u64) + Duration::from_millis(300)).await;
    let mut cancel = bob.receive_absorbing("CANCEL", &["INVITE"]).await;
    cancel.respond(200, "OK").await;

    // ── CROSSING: bob answers 200, crossing the CANCEL → reaped (ACK + BYE) ──
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    let mut bye = bob.receive_absorbing("BYE", &["INVITE"]).await;
    bye.respond(200, "OK").await;

    // ── The reroute reaches charlie via the proxy; the caller is bridged ─────
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let confirmed = call.expect(200).await;
    assert_eq!(confirmed.status, 200, "caller bridged to the reroute target");
    let mut dialog = call.ack().await;
    charlie.receive("ACK").await;

    // Clean teardown of the surviving bridged call.
    let mut d_bye = dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}
