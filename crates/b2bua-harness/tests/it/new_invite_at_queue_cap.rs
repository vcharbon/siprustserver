//! A new INVITE that finds the dispatcher's live per-call queues at their
//! global cap is refused before any call exists, with the new-call 503: a
//! To-tag, a `Retry-After` drawn over `[base, base + jitter]` so callers
//! refused together do not return together, and no `Reason`. It is counted
//! once, as a cap-shed new call, never as a dispatch drop or a discarded
//! INVITE the dispatcher answered. The call holding the cap is untouched and
//! ends properly.

use b2bua::admission::Class;
use b2bua::new_calls::Refusal;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use sip_message::header::{Reason, RetryAfter};

const RETRY_AFTER_BASE_SEC: u32 = 5;
const RETRY_AFTER_JITTER_SEC: u32 = 1_000;
/// Four draws over 1 001 values: all four equal by chance has odds of 1e-9.
const SHED_INVITES: u64 = 4;

/// One live call holds the only per-call queue. Each new INVITE is shed at
/// the cap with the new-call 503; bob is never reached.
#[tokio::test(start_paused = true)]
async fn a_new_invite_at_the_queue_cap_draws_the_capacity_503() {
    let s = B2buaScene::with_b2bua("b2bua-new-invite-at-queue-cap", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.per_call_queue_cap = 1;
            c.retry_after_base_sec = RETRY_AFTER_BASE_SEC;
            c.retry_after_jitter_sec = RETRY_AFTER_JITTER_SEC;
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holding = s.establish().await;
    settle_until(|| s.b2bua.active_calls() == 1).await;
    let metrics = s.b2bua.metrics();

    let mut retry_afters = Vec::new();
    for _ in 0..SHED_INVITES {
        let mut shed = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
        let resp = shed.expect(503).await;
        assert!(resp.to().tag().is_some(), "a non-100 final carries a To-tag (RFC 3261 §8.2.6.2)");
        assert!(
            resp.header::<Reason>().is_none(),
            "the capacity 503 carries no Reason: {:?}",
            resp.header::<Reason>()
        );
        let retry = resp.header::<RetryAfter>().expect("a Retry-After").expect("readable");
        let secs: u32 = retry.token().parse().expect("Retry-After is delta-seconds");
        assert!(
            (RETRY_AFTER_BASE_SEC..=RETRY_AFTER_BASE_SEC + RETRY_AFTER_JITTER_SEC).contains(&secs),
            "Retry-After {secs} is within [base, base + jitter]"
        );
        retry_afters.push(secs);
    }
    assert!(
        retry_afters.iter().any(|&v| v != retry_afters[0]),
        "Retry-After is jittered across shed INVITEs: {retry_afters:?}"
    );

    assert_eq!(s.b2bua.active_calls(), 1, "a shed INVITE creates no call");
    assert_eq!(metrics.cap_drops_total(), 0, "a shed INVITE is a new-call count alone");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::CapShed, Class::Normal), SHED_INVITES, "one cap shed each");
    assert_eq!(counts.rejected(Refusal::DispatchDiscard, Class::Normal), 0);
    assert_eq!(
        metrics.invite_discard_answered_total(),
        0,
        "a cap shed is not a discarded INVITE the dispatcher answered"
    );

    s.hangup(&mut holding).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one CDR, for the call that held the cap");
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
