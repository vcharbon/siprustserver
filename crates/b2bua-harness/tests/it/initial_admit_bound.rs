//! The initial route's admit is answered just past the limiter's admit
//! budget whatever the limiter does, breaker or not: a limiter with no
//! health answer that never answers an admit delays the call by that much
//! and no more, and the call runs uncounted (fail-open), still owing the
//! release of its key: the lost admit may have landed.
//!
//! The call is a full, RFC-valid call answered by the callee and hung up by
//! the caller, with one CDR and every leg reaped.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::bounded::ADMIT_SLACK;
use b2bua::limiter::LimiterEntry;
use b2bua::metrics::{LimiterFailure, LimiterOp};
use b2bua_harness::limiter::doubles::{fail_open, never_answer_admit, spy};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The stalled limiter's admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(300);

/// One hop on the harness network: the INVITE reaches the b2bua, and its
/// relay reaches bob, this long after it left.
const TRANSIT: Duration = Duration::from_millis(100);

/// Routes to bob, holding `trunk`.
fn limited_route() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The initial route's admit meets a limiter without a health answer that
/// never answers: the admit is lost just past the admit budget, counted as
/// a timeout, and bob is dialled then; the call releases the admit's key at
/// its end.
#[tokio::test(start_paused = true)]
async fn a_stalled_initial_admit_without_health_fails_open_just_past_the_admit_budget() {
    let h = Harness::new("initial-admit-stalled-no-health");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    // No health answer, never answers an admit, releases at once; records
    // the key of every admit sent and every key released.
    let limiter = spy(never_answer_admit(ADMIT_BUDGET, fail_open()));
    let b2bua = B2buaSut::builder(limited_route())
        .limiter(limiter.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let cap = ADMIT_BUDGET + ADMIT_SLACK;
    let dialled_at = TRANSIT + cap + TRANSIT;
    h.advance(dialled_at - Duration::from_millis(10)).await;
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the admit is awaited up to its cap"
    );
    h.advance(Duration::from_millis(60)).await;
    let dialled = bob.try_receive_tolerating("INVITE", &[]).await;
    let mut uas = dialled.expect("bob is dialled once the admit is lost at its cap");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    assert_eq!(
        b2bua.metrics().limiter().failures_total(LimiterOp::Admit, LimiterFailure::Timeout),
        1,
        "the lost admit is counted as a timeout"
    );
    let admitted = limiter.admitted_keys();
    assert_eq!(admitted.len(), 1, "one admit sent");
    assert_eq!(limiter.released_keys(), admitted, "the lost admit's key is released");
    let _ = h.finish().await;
}
