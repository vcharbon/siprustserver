//! A b-leg whose hop answered nothing, not even a 100, is reported dead at
//! the first-response deadline (ADR-0044) and the failure consult reroutes
//! the call. The dead leg's CANCEL stays with its transaction until a
//! provisional (RFC 3261 §9.1), so the call waits on nothing from it: once
//! the rerouted call ends, it is released at once, with its limiter hold and
//! its CDR, not at the terminating backstop.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The default first-response deadline, and a margin past it.
const DEAD_HOP: Duration = Duration::from_millis(5_300);

/// The longest the release may follow the caller's BYE: the turns that
/// answer it and the limiter release queue, far under the backstop.
const PROMPT: Duration = Duration::from_secs(1);

/// Routes to bob, holding `trunk`, with a callback context; a failure fails
/// over to carol, holding `trunk`.
fn failing_over() -> Arc<dyn CallDecisionEngine> {
    let trunk = || vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("dead-hop-ctx".into());
                r.call_limiter = trunk();
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                assert_eq!(req.failure.timeout_kind.as_deref(), Some("response"));
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = trunk();
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// bob answers nothing; carol, dialled on the failure consult, answers;
/// alice hangs up. The call is released, its hold freed and its one CDR
/// written within [`PROMPT`] of her BYE, and bob is never CANCELed.
#[tokio::test(start_paused = true)]
async fn a_rerouted_call_whose_first_hop_was_dead_is_released_when_it_ends() {
    let h = Harness::new("dead-hop-reroute-release");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over())
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let _dead = bob.receive("INVITE").await;
    h.advance(DEAD_HOP).await;
    let mut uas = carol.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;

    let hung_up = tokio::time::Instant::now();
    scenario_harness::callflow::hangup(&mut dialog, &carol).await;
    settle_until(|| b2bua.is_reaped() && !b2bua.cdr_records().is_empty()).await;
    let released = hung_up.elapsed();
    assert!(b2bua.is_reaped(), "the call is released when it ends");
    assert!(released < PROMPT, "released {released:?} after the BYE, not at the backstop");
    assert_eq!(b2bua.cdr_records().len(), 1, "with its one CDR");
    let count = b2bua.limiter_count();
    assert!(count.admitted > 0, "the routes held `trunk`");
    assert_eq!(count.released, count.admitted, "every hold released with the call");
    b2bua.assert_fully_reaped();

    let bob_addr = bob.addr();
    let report = h.finish().await;
    assert!(
        !report.entries().iter().any(|e| e.to == bob_addr && e.raw.starts_with(b"CANCEL ")),
        "no CANCEL to a hop that answered nothing (RFC 3261 §9.1)"
    );
}
