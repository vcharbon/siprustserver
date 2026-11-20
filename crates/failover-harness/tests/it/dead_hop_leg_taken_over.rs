//! **A dead hop's leg holds no release on the copy that takes the call
//! over.** The callee first dialled answers nothing, not even a 100: its hop
//! is reported dead at the first-response deadline (ADR-0044), the failure
//! consult reroutes the call, and the dead leg's CANCEL is held for a
//! provisional (RFC 3261 §9.1). The record says so, and replicates. When the
//! serving node dies, the survivor rebuilds the dead leg's transaction with
//! that CANCEL held (ADR-0058) and serves the caller's BYE: the call waits on
//! nothing from the dead hop and is released at once, with one CDR, and no
//! CANCEL ever reaches the hop that answered nothing.
//!
//! Beyond the failover sweep: a rerouted call with a dead first hop, taken over.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use failover_harness::{total_cdrs_for, worker_ordinals, FailoverHarness, WorkerHealth};

use crate::departure_support::bring_up_deciding;
use crate::inflight_resend::{settle_to_one_cdr, split_mut, survivor_of, REPLICATE};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const CAROL: &str = "127.0.0.1:5071";

/// The default first-response deadline, and a margin past it.
const DEAD_HOP: Duration = Duration::from_millis(5_300);

/// The longest the CDR may follow the caller's BYE: the turns that relay and
/// answer it, far under the terminating backstop.
const PROMPT_MS: i64 = 1_000;

/// Past the end of the dead hop's INVITE transaction the survivor rebuilt:
/// Timer B, then the give-up's 64·T1 hold (RFC 3261 §17.1.1.2).
const TXN_OVER: Duration = Duration::from_secs(70);

/// Routes to bob with a callback context; a failure fails over to carol.
fn failing_over() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("dead-hop-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|req| {
                assert_eq!(req.failure.timeout_kind.as_deref(), Some("response"));
                CallTreatment::Route(route_to("127.0.0.1", 5071))
            })
            .build(),
    )
}

#[tokio::test(start_paused = true)]
async fn a_taken_over_call_whose_first_hop_was_dead_is_released_when_it_ends() {
    let name = "dead-hop-leg-taken-over";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let carol = fh.agent("carol", CAROL).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, failing_over()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let dead = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(dead.request());
    fh.advance(DEAD_HOP).await;
    let mut uas = carol.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    fh.advance(REPLICATE).await;
    let call_ref = survivor_of(&pri_ord, &w_b1, &w_b2).scan_one_backed_up(&pri_ord).await;
    let call_ref = call_ref.expect("the rerouted call replicated");

    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "the serving node dies with the dead hop's CANCEL held");
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor_of(&pri_ord, &w_b1, &w_b2).simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    let hung_up = fh.now_ms();
    scenario_harness::callflow::hangup(&mut dialog, &carol).await;
    while total_cdrs_for(&[&w_b1, &w_b2], &call_ref) == 0 && fh.now_ms() - hung_up < PROMPT_MS {
        fh.advance(Duration::from_millis(50)).await;
    }
    let released = fh.now_ms() - hung_up;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "the survivor writes the CDR");
    assert!(released < PROMPT_MS, "released {released} ms after the BYE, not at the backstop");

    fh.advance(TXN_OVER).await;
    settle_to_one_cdr(&fh, &[&alice, &bob, &carol], &[&w_b1, &w_b2], &call_ref).await;
    let bob_addr = bob.addr();
    let cancelled =
        fh.sip_entries().iter().any(|e| e.to == bob_addr && e.raw.starts_with(b"CANCEL "));
    assert!(!cancelled, "no CANCEL to a hop that answered nothing (RFC 3261 §9.1)");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
