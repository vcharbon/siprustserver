//! **Response↔transaction Via correlation across reroute and glare, via-LB**
//! (newkahneed/014). Two flows that are clean direct but were flagged by the
//! downstream `rfc3261.via` audit once a REAL `sip_proxy::ProxyCore` relays
//! both directions of the b-leg:
//!
//!   alice :5060 ─▶ proxy :5080 ─▶ b1 :5091 ─▶ proxy :5080 ─▶ bob :5070
//!
//! 1. **486-then-reroute** — the first b-leg answers 486; the worker auto-ACKs
//!    it (same branch as the INVITE, §17.1.1.3, riding the INVITE's preloaded
//!    outbound-proxy Route) and immediately forks the reroute INVITE toward the
//!    SAME next hop (the proxy).
//! 2. **BYE/BYE glare** — the worker's teardown BYE (relaying alice's) crosses
//!    the callee's own BYE on the b-leg; both directions of the SAME Call-ID
//!    carry a BYE at the SAME CSeq number (RFC 3261 §12.2.1.1 lets each side
//!    pick any initial CSeq, so the coincidence is legal and must not corrupt
//!    response→client-transaction matching at the relay).
//!
//! These are audit-gated end to end (the harness hard-gates every bind's trace
//! through `sip_net::rfc_audit`), so the tests pin BOTH halves of the 014 ask:
//! every worker/proxy transaction toward a shared next hop presents a distinct
//! top-Via branch, and each response correlates (per §17.1.3, BY BRANCH) to
//! exactly the client transaction that sent it.

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
const CHARLIE: &str = "127.0.0.1:5071"; // reroute target
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";

/// Failover-capable decision: route to bob, and on failure reroute to charlie.
fn reroute_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("via-lb-486-reroute-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.new_ruri = Some("sip:127.0.0.1:5071".into());
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// **486-then-reroute (bc_02_bl_reroutes_on_486__via_lb twin).** Bob rejects
/// with 486; the worker ACKs the failed b-leg and forks the reroute INVITE to
/// charlie through the same proxy hop; alice is bridged to charlie. The
/// recorded traces (alice, bob, charlie, proxy, worker) must pass the full RFC
/// audit — in particular `rfc3261.via` response↔transaction correlation at the
/// relay bind that carried the 486-leg INVITE, its ACK, and the reroute INVITE
/// back to back.
#[tokio::test(start_paused = true)]
async fn reroute_on_486_via_lb_keeps_response_txn_correlation() {
    let mut fh = FailoverHarness::new("via-lb-486-reroute", &["b1"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let charlie = fh.agent("charlie", CHARLIE).await;

    let proxy = fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap())]).await;
    let b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &[],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            reroute_decision(),
            Arc::new(NoopLimiter),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready(), "the lone worker is ready at steady state");

    // ── alice → proxy → b1 → proxy → bob ─────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;

    // Bob rejects: 486. The worker auto-ACKs (INVITE's branch + Route, so the
    // ACK egresses through the proxy too) and consults /call/failure.
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;

    // ── The reroute INVITE reaches charlie via the proxy; alice is bridged ───
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

    // Full per-bind suite — including the relay bind's own client-transaction
    // correlation (`rfc3261.via`), which the Drop-time endpoint gate skips.
    fh.assert_full_rfc_clean("via-lb-486-reroute");
    drop(proxy);
}

/// **BYE/BYE glare (bc_rc_bye_then_bye__via_lb twin).** An established via-LB
/// call; alice hangs up, and the callee's own BYE crosses the worker's relayed
/// BYE on the b-leg. Bob's CSeq space is aligned so both crossing BYEs carry
/// the SAME (Call-ID, CSeq, method) — the relay must still keep each 200
/// matched to exactly its client transaction (distinct top-Via branches,
/// §17.1.3). Everything is audit-gated at drop.
#[tokio::test(start_paused = true)]
async fn bye_bye_glare_via_lb_keeps_response_txn_correlation() {
    let mut fh = FailoverHarness::new("via-lb-bye-glare", &["b1"]);

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
            Arc::new(
                ScriptedDecisionEngine::builder()
                    .fallback(|_| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
                    .build(),
            ),
            Arc::new(NoopLimiter),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready(), "the lone worker is ready at steady state");

    // ── Establish alice ⇄ bob through the proxy ──────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let confirmed = call.expect(200).await;
    assert_eq!(confirmed.status, 200);
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── GLARE: both sides hang up in the same instant ────────────────────────
    // The worker's b-leg BYE rides CSeq 2 (INVITE was 1). Align bob's local
    // CSeq space so his own BYE is ALSO CSeq 2 (§12.2.1.1 — any initial local
    // sequence number is legal): the two crossing BYEs then collide on
    // (Call-ID, CSeq, method) and only the top-Via branch disambiguates their
    // 200s at the relay.
    let mut bob_dialog = bob_uas.dialog();
    bob_dialog.set_local_cseq(1);

    let mut a_bye = dialog.bye().await; // alice → proxy → b1; b1 relays BYE → proxy → bob
    let mut worker_bye = bob.receive("BYE").await;
    // Bob fires his OWN BYE before answering the worker's — the two hangup
    // transactions are now genuinely concurrent on the b-leg, and bob's 200
    // (echoing the worker-BYE's Via stack) reaches the relay AFTER it forwarded
    // bob's same-(Call-ID, CSeq, method) BYE toward the worker.
    let mut b_bye = bob_dialog.bye().await; // bob → proxy → b1 (crosses the relayed BYE)
    worker_bye.respond(200, "OK").await;

    // Both hangup transactions complete: each 200 must reach exactly its
    // originator's client transaction back through the relay.
    a_bye.expect(200).await;
    b_bye.expect(200).await;

    // Full per-bind suite — including the relay bind's own client-transaction
    // correlation (`rfc3261.via`), which the Drop-time endpoint gate skips.
    fh.assert_full_rfc_clean("via-lb-bye-glare");
    drop(proxy);
}

