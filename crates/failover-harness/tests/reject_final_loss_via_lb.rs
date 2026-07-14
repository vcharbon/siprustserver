//! **Rejected-call recovery under packet loss, through the REAL front proxy.**
//!
//! The regression this pins (found by the loadgen loss soak): the LB proxy used
//! to synthesize the RFC 3261 §17.1.1.3 hop-by-hop ACK the moment it relayed a
//! non-2xx INVITE final upstream. That ACK quenched the worker's §17.2.1
//! Timer G — the ONLY retransmitter of the final in the whole system (the
//! worker's auto-100 already silences the caller's INVITE Timer A) — while the
//! relay itself stays exactly-once (the proxy is transaction-less, ADR-0022
//! X4). Lose that single relayed copy and the caller never sees the reject at
//! all: nothing retransmits, the caller wedges to Timer B / the 32 s safety
//! ceiling, the call lands `torn_down`.
//!
//!   alice :5060 ─▶ proxy :5080 ─▶ b1 :5091 ─▶ proxy :5080 ─▶ bob :5070
//!                  (real LoadBalancer)   (B2buaCore; b-leg via the proxy)
//!
//! The fix makes reliability END-TO-END: the proxy never ACKs a relayed final;
//! the worker's Timer G retransmits it through the stateless relay until the
//! caller's own ACK arrives, and the proxy relays that ACK downstream on the
//! INVITE's remembered hop (same target, same outbound branch) so the worker's
//! server transaction matches it and stops retransmitting.
//!
//! Here bob rejects with `603 Decline`; a deterministic pre-ingress hook on
//! alice's bind swallows the FIRST relayed copy of the 603. The test then
//! proves the full recovery loop: Timer G re-sends at ~500 ms → the proxy
//! relays the retransmit → alice receives + ACKs → the ACK crosses the proxy
//! back to the worker → Timer G is quenched (no third copy) → the call reaps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use failover_harness::FailoverHarness;
use sip_net::PreIngressAction;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";

fn route_decision(dest_port: u16) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(route_to("127.0.0.1", dest_port)))
            .build(),
    )
}

#[tokio::test(start_paused = true)]
async fn lost_relayed_603_recovers_via_worker_timer_g_and_relayed_ack() {
    let mut fh = FailoverHarness::new("reject-final-loss-via-lb", &["b1"]);

    // Deterministic loss model on ALICE's bind: swallow the FIRST arriving 603
    // and count every copy, so the tail of the test can assert both the
    // retransmit delivery AND the post-ACK quench (exactly two copies ever
    // reach the bind — the dropped one and the recovered one).
    let copies_at_alice = Arc::new(AtomicUsize::new(0));
    let counter = copies_at_alice.clone();
    let alice = fh
        .agent_with_pre_ingress(
            "alice",
            ALICE,
            Arc::new(move |bytes: &[u8], _src, _depth| {
                if bytes.starts_with(b"SIP/2.0 603") {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        return PreIngressAction::Drop;
                    }
                }
                PreIngressAction::Accept
            }),
        )
        .await;
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
            route_decision(5070),
            Arc::new(NoopLimiter),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready(), "the lone worker is ready at steady state");

    // ── alice → proxy → b1 → proxy → bob; bob REJECTS ────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(603, "Decline").await;

    // The worker's §17.1.1.3 auto-ACK for bob's 603 crosses the proxy to bob
    // (relayed on the b-leg INVITE's remembered hop — NOT proxy-synthesized).
    bob.receive_absorbing("ACK", &["INVITE"]).await;

    // Meanwhile the worker relayed the 603 to alice; her first copy was
    // swallowed by the loss hook. NOTHING may quench the worker's Timer G —
    // advance to its first firing (~500 ms after the final) so the worker
    // re-sends and the stateless proxy relays the retransmit.
    fh.advance(Duration::from_millis(600)).await;
    assert_eq!(
        copies_at_alice.load(Ordering::SeqCst),
        2,
        "expected the dropped first copy + the Timer G retransmit at alice's bind"
    );

    // Alice receives the recovered 603; the agent auto-ACKs it (§17.1.1.3).
    let rejected = call.expect(603).await;
    assert_eq!(rejected.status, 603, "the reject reaches the caller despite the lost first copy");

    // The ACK relays through the proxy to the worker on the INVITE's own hop,
    // confirming the server transaction. Advance PAST where Timer G would fire
    // again (doubling: next re-send ~1.5 s after the final) — a third copy at
    // alice's bind means the relayed ACK failed to quench the retransmitter.
    fh.advance(Duration::from_millis(1_700)).await;
    assert_eq!(
        copies_at_alice.load(Ordering::SeqCst),
        2,
        "the caller's relayed ACK must quench the worker's Timer G (no further 603 retransmits)"
    );

    // The rejected call is fully reaped on the worker.
    let reaped = fh
        .settle_terminal(|| async {
            b1.metrics().removals_total() == b1.metrics().creations_total() && b1.memory_clean()
        })
        .await;
    assert!(
        reaped,
        "the rejected call must be fully reaped (creations {} != removals {}, or memory not \
         clean: {} live / {} locks)",
        b1.metrics().creations_total(),
        b1.metrics().removals_total(),
        b1.active_calls(),
        b1.lock_count(),
    );
    drop(proxy);
}
