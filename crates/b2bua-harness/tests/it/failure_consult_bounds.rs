//! What bounds a `/call/failure` consult on a healthy node, against its
//! answer's deadline (ADR-0039):
//!
//! - the failover route's admit is answered just past the limiter's admit
//!   budget whatever the limiter does, so a stalled limiter delays the
//!   failover by that much and no more — well before the deadline;
//! - with the decision deadline off the consult has no deadline, and its
//!   fold still reaches the rules: the failover leg is dialled.
//!
//! Each call is a full, RFC-valid call answered by the failover target and
//! hung up by the caller, with one CDR and every leg reaped.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::bounded::ADMIT_SLACK;
use b2bua::limiter::LimiterEntry;
use b2bua_harness::limiter::doubles::{fail_open, never_answer_admit};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The stalled limiter's admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(300);

/// Routes to bob with a callback context, holding nothing; a failure fails
/// over to carol, holding `trunk`.
fn failing_over() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("failover-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// Bob refuses alice's call; the instant just after his 486.
async fn bob_refuses(
    alice: &Agent,
    bob: &Agent,
    b2bua: &B2buaSut,
) -> (scenario_harness::ClientInvite, tokio::time::Instant) {
    let call = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    (call, tokio::time::Instant::now())
}

/// The failover route's admit meets a limiter that never answers: the fold
/// lands just past the admit budget (the admit lost), and carol is dialled
/// then — not at the consult's deadline.
#[tokio::test(start_paused = true)]
async fn a_stalled_failover_admit_lands_the_fold_just_past_the_admit_budget() {
    let h = Harness::new("failure-consult-stalled-admit");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over())
        .limiter(never_answer_admit(ADMIT_BUDGET, fail_open()))
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let (call, refused_at) = bob_refuses(&alice, &bob, &b2bua).await;
    let cap = (ADMIT_BUDGET + ADMIT_SLACK).as_millis() as i64;
    h.advance(Duration::from_millis(cap as u64 - 10)).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the admit is awaited up to its cap"
    );
    h.advance(Duration::from_millis(60)).await;
    let dialled = carol.try_receive_tolerating("INVITE", &[]).await;
    let mut uas = dialled.expect("carol is dialled once the admit is lost at its cap");
    assert!(refused_at.elapsed().as_millis() as i64 <= cap + 60, "within the cap of the refusal");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    finish_answered(h, call, &carol, &b2bua).await;
}

/// Carol answered the failover leg: alice is connected, then hangs up; one
/// CDR, every leg reaped.
async fn finish_answered(
    h: Harness,
    mut call: scenario_harness::ClientInvite,
    carol: &Agent,
    b2bua: &B2buaSut,
) {
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, carol).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// With the decision deadline off, the consult has no answer deadline: its
/// fold names none and reaches the rules — carol is dialled.
#[tokio::test(start_paused = true)]
async fn an_unbounded_failure_consult_s_fold_reaches_the_rules() {
    let h = Harness::new("failure-consult-unbounded");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over())
        .tune(|c| c.call_control_timeout_ms = 0)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let (call, _) = bob_refuses(&alice, &bob, &b2bua).await;
    carol.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    finish_answered(h, call, &carol, &b2bua).await;
}
