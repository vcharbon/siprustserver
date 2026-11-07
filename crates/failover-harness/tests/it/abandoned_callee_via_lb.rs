//! **A callee the worker abandons, through a REAL front proxy (via-LB).**
//!
//! The internally-originated CANCEL and crossing-200 reap of
//! `cancel_200_crossing_internal.rs`, exercised across a real
//! `sip_proxy::ProxyCore` between the caller and the worker AND between the
//! worker and the callee (the b-leg-through-the-LB topology,
//! `B2BUA_OUTBOUND_PROXY`):
//!
//!   alice :5060 ─▶ proxy :5080 ─▶ b1 :5091 ─▶ proxy :5080 ─▶ bob :5070
//!                  (real LoadBalancer)   (B2buaCore; b-leg via the proxy)
//!
//! The worker's own deadlines abandon the callee: the ring deadline of one
//! that rang, the setup deadline of one that sent a 100 or nothing, the
//! first-response deadline (ADR-0044) of one that sent nothing. The stateless
//! proxy relays every 100 (RFC 3261 §16.11), so the caller hears the worker's
//! 100 and the worker hears the callee's:
//!
//! - **A callee that answered with a 100 or an 18x is CANCELed** when it is
//!   abandoned: its branch is `Proceeding`, the CANCEL goes on the wire, and
//!   the callee's 487 resolves the leg.
//! - **A callee that sent nothing holds the CANCEL** (§9.1: the branch drew no
//!   provisional). The call outlives it — a `Cancelling` leg is not resolved
//!   (`call::helpers::leg_is_resolved`) — so a `200` the callee sends later,
//!   promptly or seconds after, is reaped by `cancel-200-crossing` (ACK + BYE
//!   to the late callee) and the caller still gets its final.
//!
//! Beyond the failover sweep: a lone worker's own deadlines abandoning a callee through the proxy,
//! with no node fault.

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
const CHARLIE: &str = "127.0.0.1:5071"; // the second target, after a failure
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";

/// Every deadline that abandons the callee here, seconds: short, to keep the
/// paused-clock advance (and the b-leg's Timer A retransmit churn) small, and
/// far below the configured initial-INVITE bound (default 158 s).
const ABANDON_SEC: u64 = 5;

/// The worker's setup deadline at [`ABANDON_SEC`], its first-response
/// deadline at Timer B past it: what abandons a callee that never rings, the
/// caller answered 408.
fn setup_deadline(c: &mut b2bua::B2buaConfig) {
    c.setup_timeout_ms = ABANDON_SEC as i64 * 1000;
    c.invite_first_response_timeout_sec = b2bua::B2buaConfig::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC;
}

/// The worker's first-response deadline at [`ABANDON_SEC`] (ADR-0044): what
/// abandons a callee that sends nothing at all, consulting the failure.
fn first_response_deadline(c: &mut b2bua::B2buaConfig) {
    c.invite_first_response_timeout_sec = ABANDON_SEC as i64;
}

/// Decision that routes every call to `dest_port`, with no `callback_context`:
/// a failure is not consulted.
fn reject_decision(dest_port: u16) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(route_to("127.0.0.1", dest_port)))
            .build(),
    )
}

/// Failover-capable decision: a ring deadline of [`ABANDON_SEC`] plus a
/// `callback_context` (the call stays Active for the `/call/failure`
/// consult), sending the call to charlie (5071) on failure.
fn retarget_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_ms = Some(ABANDON_SEC as i64 * 1000);
                r.callback_context = Some("abandon-retarget-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                // Behind the LB the b-leg is forwarded to its Request-URI, and a
                // second-target leg's R-URI defaults to the a-leg's (bob). Name the second
                // target explicitly so the proxy forwards to charlie, not
                // back to bob (the `test_adapter` anti-loop invariant).
                let mut r = route_to("127.0.0.1", 5071);
                r.new_ruri = Some("sip:127.0.0.1:5071".into());
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// The lone worker, its b-leg sent through the proxy.
async fn spawn_b1(
    fh: &mut FailoverHarness,
    decision: Arc<dyn CallDecisionEngine>,
) -> failover_harness::ReplicatedB2buaSut {
    fh.spawn_worker_limited(
        "b1",
        "b1",
        B1,
        &[],
        ("127.0.0.1", 5070),
        ("127.0.0.1", 5080),
        decision,
        Arc::new(NoopLimiter),
    )
    .await
}

/// Wait for the single worker to fully reap the call: `creations == removals`
/// (call removed) and per-call memory clean (no live call, no lingering lock),
/// with the call's one CDR written and no trace of it left.
async fn assert_reaped(fh: &FailoverHarness, b1: &failover_harness::ReplicatedB2buaSut) {
    let reaped = fh
        .settle_terminal(|| async {
            b1.metrics().removals_total() == b1.metrics().creations_total() && b1.memory_clean()
        })
        .await;
    assert!(
        reaped,
        "the abandoned-callee call must be fully reaped (creations {} != removals {}, or \
         memory not clean: {} live / {} locks)",
        b1.metrics().creations_total(),
        b1.metrics().removals_total(),
        b1.active_calls(),
        b1.lock_count(),
    );
    let cdrs = b1.cdr_records();
    assert_eq!(cdrs.len(), 1, "the lone worker writes the call's one CDR");
    failover_harness::assert_call_ended(&[b1], &cdrs[0].call_ref, 1, "one call, one worker").await;
}

/// **Reject path, a callee that answered with a 100 alone.** The caller hears
/// the worker's 100 through the proxy; the callee's 100 reaches the worker, so
/// when the setup deadline abandons the callee the CANCEL goes on the wire at
/// once: the callee answers it 200 and its INVITE 487, and the caller gets its
/// final.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn trying_only_callee_abandoned_via_lb__reject__is_cancelled() {
    let mut fh = FailoverHarness::new("s10b-trying-callee-abandoned-reject", &["b1"])
        .with_worker_tune(setup_deadline);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = spawn_b1(&mut fh, reject_decision(5070)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(100, "Trying").await;

    fh.advance(Duration::from_secs(ABANDON_SEC) + Duration::from_millis(300)).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    bob_uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    let failed = call.expect(408).await;
    assert_eq!(failed.status(), 408, "caller's INVITE resolves at the setup deadline");
    fh.advance(Duration::from_millis(10)).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **Second-target path, a callee that rang.** Its ring deadline CANCELs the callee
/// on the wire, the callee's 487 resolves the abandoned leg, and the leg
/// to the second target bridges the caller.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn ringing_callee_no_answer_via_lb__retarget__is_cancelled_and_retargeted() {
    let mut fh = FailoverHarness::new("s10b-ringing-callee-noanswer-retarget", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let charlie = fh.agent("charlie", CHARLIE).await;
    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = spawn_b1(&mut fh, retarget_decision()).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    fh.advance(Duration::from_secs(ABANDON_SEC) + Duration::from_millis(300)).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    bob_uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    charlie.receive("ACK").await;
    let mut d_bye = dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **Reject path, a callee that sent nothing, prompt crossing.** The setup
/// deadline abandons the callee, the b-leg's CANCEL is held (the callee sent
/// no provisional), and the callee answers `200` crossing it. The abandoned
/// callee MUST be reaped (ACK + BYE via the proxy) and the caller MUST still
/// get its final.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn silent_callee_abandoned_via_lb__reject__reaps_crossing_200() {
    let mut fh = FailoverHarness::new("s10b-silent-callee-abandoned-reject", &["b1"])
        .with_worker_tune(setup_deadline);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = spawn_b1(&mut fh, reject_decision(5070)).await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready(), "the lone worker is ready at steady state");

    // ── alice → proxy → b1 → proxy → bob ─────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;

    // The SILENT callee: it takes the b-leg INVITE and sends nothing at all.
    let mut bob_uas = bob.receive("INVITE").await;

    // ── Trip the worker's setup deadline → CANCEL the b-leg ──────────────────
    fh.advance(Duration::from_secs(ABANDON_SEC) + Duration::from_millis(300)).await;

    // The worker's CANCEL is HELD (RFC 3261 §9.1): the b-leg branch is
    // response-less.
    // The call MUST still outlive the held CANCEL: the b-leg sits
    // `Cancelling`, unresolved.

    // ── CROSSING: bob answers 200 OK against the held CANCEL ─────────────────
    // The 2xx ends the b-leg client txn (clearing the held CANCEL — moot) and
    // the abandoned callee MUST be reaped — ACK then immediate BYE via the proxy.
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    let mut bye = bob.receive_absorbing("BYE", &["INVITE"]).await;
    bye.respond(200, "OK").await;

    // The caller holds its final (authored in the terminating turn) — NOT
    // dropped on a removed call.
    let failed = call.expect(408).await;
    assert_eq!(failed.status(), 408, "caller's INVITE resolves with a final failure");
    // Flush alice's §17.1.1.3 ACK for the 408 (sent by the receive above) the
    // one hop to the proxy before the Drop-time RFC gate snapshots the trace.
    fh.advance(Duration::from_millis(10)).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **Reject path, a callee that sent nothing, DELAYED crossing.** Identical to
/// the prompt case, but the crossing `200` is delivered many seconds AFTER the
/// worker's CANCEL was held (well within the ~32 s terminating-safety
/// backstop). A `Cancelling` leg stays unresolved, so the call is alive across
/// the gap and the reap holds whenever the `200` arrives.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn silent_callee_abandoned_via_lb__reject__delayed_crossing_200_still_reaped() {
    let mut fh = FailoverHarness::new("s10b-silent-callee-abandoned-reject-delayed", &["b1"])
        .with_worker_tune(setup_deadline);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = spawn_b1(&mut fh, reject_decision(5070)).await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;

    fh.advance(Duration::from_secs(ABANDON_SEC) + Duration::from_millis(300)).await;
    // The CANCEL is HELD (§9.1 — the branch drew no provisional) and never
    // reaches bob. The call stays HELD
    // Terminating (b-leg `Cancelling`, awaiting a final) — the
    // timing-independence under test.
    fh.advance(Duration::from_secs(10)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "no CANCEL toward a branch that drew no provisional (§9.1)"
    );

    // ── CROSSING (late): bob finally answers 200 → still reaped ──────────────
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    let mut bye = bob.receive_absorbing("BYE", &["INVITE"]).await;
    bye.respond(200, "OK").await;

    let failed = call.expect(408).await;
    assert_eq!(
        failed.status(),
        408,
        "caller's INVITE resolves with a final failure after a late reap"
    );
    // Flush alice's §17.1.1.3 ACK for the 408 the one hop to the proxy before
    // the Drop-time RFC gate snapshots the trace.
    fh.advance(Duration::from_millis(10)).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **Second-target path, crossing.** The via-LB sibling of
/// `no_answer_cancel_crossed_by_200_reaps_the_abandoned_callee_and_failover_proceeds`
/// (which is direct-topology). A `callback_context` keeps the call Active while
/// `/call/failure` is consulted at the first-response deadline; the crossing
/// `200` on the abandoned callee is reaped (ACK + BYE via the proxy) AND the
/// leg to the second target proceeds — the caller is bridged. The callee
/// sent nothing, so the CANCEL is held until its `200`.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn silent_callee_abandoned_via_lb__retarget__reaps_crossing_200_and_retargets() {
    let mut fh = FailoverHarness::new("s10b-silent-callee-abandoned-retarget", &["b1"])
        .with_worker_tune(first_response_deadline);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await; // silent, abandoned, answers late
    let charlie = fh.agent("charlie", CHARLIE).await; // the second target

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;

    let b1 = spawn_b1(&mut fh, retarget_decision()).await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;

    // The first-response deadline fires → CANCEL bob (held — §9.1, the branch
    // is response-less) + /call/failure consult (call stays Active).
    fh.advance(Duration::from_secs(ABANDON_SEC) + Duration::from_millis(300)).await;

    // ── CROSSING: bob answers 200 against the held CANCEL → reaped (ACK+BYE) ─
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    let mut bye = bob.receive_absorbing("BYE", &["INVITE"]).await;
    bye.respond(200, "OK").await;

    // ── The second-target INVITE reaches charlie via the proxy; the caller is bridged ─────
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let confirmed = call.expect(200).await;
    assert_eq!(confirmed.status(), 200, "caller bridged to the second target");
    let mut dialog = call.ack().await;
    charlie.receive("ACK").await;

    // Clean teardown of the surviving bridged call.
    let mut d_bye = dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    assert_reaped(&fh, &b1).await;
    drop(proxy);
}

/// **A 2xx after the call is gone, from a callee writing its own Record-Route.**
/// The caller's CANCEL ends the call and the callee answers its INVITE anyway,
/// past the worker's safety bound, so the transaction closes its obligations
/// alone (ADR-0034). The ACK and the BYE it builds leave for the proxy, which
/// carries them to the hop the callee recorded (itself, here): the worker reaches
/// no peer pod-direct.
#[tokio::test(start_paused = true)]
#[allow(non_snake_case)]
async fn a_2xx_after_the_call_ended_via_lb__leaves_for_the_proxy_whatever_the_callee_recorded() {
    let mut fh = FailoverHarness::new("orphan-first-hop-via-lb", &["b1"]);
    let why = "bob plays a callee that records itself and answers past a CANCEL, this test's input";
    fh.allow_rfc_violation("no-record-route-from-ua", why);
    fh.allow_rfc_violation("no-200-after-cancel", why);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = spawn_b1(&mut fh, reject_decision(5070)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    // Past the worker's safety bound on a leg whose final never comes.
    fh.advance(Duration::from_secs(40)).await;
    assert_eq!(b1.active_calls(), 0, "the call ended; its transaction is orphaned");

    bob_uas
        .respond(200, "OK")
        .with_header("Record-Route", &format!("<sip:{BOB};lr>"))
        .with_sdp(ANSWER)
        .await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    bob.receive_absorbing("BYE", &["INVITE"]).await.respond(200, "OK").await;
    fh.advance(Duration::from_millis(50)).await;

    let b1_addr: std::net::SocketAddr = B1.parse().unwrap();
    let proxy_addr: std::net::SocketAddr = PROXY.parse().unwrap();
    for entry in fh.sip_entries().iter().filter(|e| e.from == b1_addr) {
        assert_eq!(entry.to, proxy_addr, "b1 sent {} around the proxy", entry.start_line());
    }
    assert_reaped(&fh, &b1).await;
    drop(proxy);
}
