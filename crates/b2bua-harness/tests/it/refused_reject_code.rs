//! A reject decision rejects only with a non-2xx final (RFC 3261 §21.3–§21.6).
//! One stating a provisional, a 2xx or a code past 699 is no rejection: the
//! caller gets the plain server error, as for a redirect decision that is not
//! a 3xx, and the callee is never reached.

use std::sync::Arc;

use b2bua::decision::{CallTreatment, RejectDecision, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

fn reject(code: u16) -> CallTreatment {
    CallTreatment::Reject(RejectDecision {
        reject_code: code,
        reject_reason: None,
        update_headers: None,
        service_ext: Default::default(),
        label: None,
    })
}

/// A new call the decision rejects with `code`; the caller's final is `expected`.
async fn initial_reject(code: u16, expected: u16) {
    let h = Harness::new(format!("initial-reject-{code}"));
    let alice = h.agent("alice", "127.0.0.1:7150").await;
    let bob = h.agent("bob", "127.0.0.1:7151").await;
    let engine = ScriptedDecisionEngine::builder().fallback(move |_req| reject(code)).build();
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:7152").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let answered = call.expect(expected).await;
    assert_eq!(answered.status(), expected, "reject decision {code}");

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_reject_decision_stating_a_provisional_is_refused() {
    initial_reject(180, 500).await;
}

#[tokio::test(start_paused = true)]
async fn a_reject_decision_stating_a_2xx_is_refused() {
    initial_reject(200, 500).await;
}

#[tokio::test(start_paused = true)]
async fn a_reject_decision_stating_a_code_past_699_is_refused() {
    initial_reject(700, 500).await;
}

/// The ordinary path the refusal must not break: a 486 reject is answered as stated.
#[tokio::test(start_paused = true)]
async fn a_reject_decision_stating_a_failure_final_is_answered() {
    initial_reject(486, 486).await;
}
