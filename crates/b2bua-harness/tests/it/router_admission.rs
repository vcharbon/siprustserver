//! The panic-ELU and CPS bucket rungs of the admission ladder, end-to-end
//! through a running `B2buaCore`: a new-dialog INVITE is judged by the
//! panic-ELU backstop, then the CPS token bucket, at router ingress, before any
//! call/dialog state or per-call queue exists. The rungs' UNIT behaviour
//! (bucket drain/refill, panic-ELU, emergency admits on an empty bucket) is
//! pinned in `b2bua::admission::tests` and `b2bua::overload::tests`; this
//! file proves the WIRING — the verdict turns into a real 503 on the wire,
//! sent through the INVITE server transaction (the new-call reject: a
//! `Retry-After`, no `Reason`), no per-call resources are born for a reject,
//! emergency bypasses the empty bucket, and an admit advances the published
//! `adm`.
//!
//! Real-clock except the emergency no-debt test: a size-0 bucket rejects/admits
//! the FIRST INVITE with no timer to wait on. The no-debt test waits for one
//! refill, so it runs paused and advances the clock by exactly that interval.

use b2bua::admission::Class;
use b2bua::new_calls::Refusal;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::header::{Reason, RetryAfter};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A non-emergency new INVITE against a worker whose CPS bucket is exhausted
/// (capacity 0, no refill) is refused at router ingress: the caller gets the
/// new-call `503` through its server transaction — a `Retry-After`, no
/// `Reason` (ADR-0037 item 3) — and NO per-call state is created — no CDR, no
/// live call, and bob is never contacted.
#[tokio::test]
async fn cps_bucket_empty_503s_a_new_invite_before_any_call_state() {
    let h = Harness::with_transit_delay("b2bua-router-admission-bucket-empty", 0)
        .describe("an exhausted CPS bucket refuses a new INVITE with the new-call 503");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    // bob exists only to prove he is NEVER reached on a reject.
    let _bob = h.agent("bob", "127.0.0.1:5073").await;
    // Capacity 0 + rate 0 → the bucket is empty and never refills, so the very
    // first non-emergency INVITE is rejected deterministically.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5073)
        .tune(|c| {
            c.cps_bucket_size = 0;
            c.cps_bucket_rate = 0;
        })
        .start(&h, "b2bua", "127.0.0.1:5083")
        .await;

    let mut call = alice.invite(&_bob).with_sdp(OFFER).through(b2bua.addr).send().await;

    // The bucket rung refuses: the caller's INVITE gets a 503 (after the txn
    // layer's absorbed auto-100). It carries a Retry-After, no Reason, and a
    // To-tag (this codebase tags every non-100 final); the rejecting INVITE
    // server txn absorbs the ACK, and no call state exists.
    let resp = call.expect(503).await;
    assert!(resp.header::<Reason>().is_none(), "a new-call 503 carries no Reason");
    assert!(resp.header::<RetryAfter>().is_some(), "overload 503 must carry a Retry-After hint");
    assert!(resp.to().tag().is_some(), "non-100 final carries a To-tag (RFC §8.2.6.2)");

    // No per-call state was created for the rejected INVITE: no live call, and the
    // worker counted it once as a new call refused for an empty bucket.
    settle_until(|| b2bua.new_calls().rejected(Refusal::BucketEmpty, Class::Normal) == 1).await;
    assert_eq!(b2bua.active_calls(), 0, "a rejected INVITE creates no live call");
    assert!(
        b2bua.cdr_records().is_empty(),
        "a refused INVITE writes no CDR (no call was ever born)"
    );
    // The reject is NOT counted on the published `adm` (only admitted
    // non-emergency new dialogs are).
    assert_eq!(b2bua.overload().metrics().non_emergency_admitted_total, 0);

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

/// A refusal by the bucket leaves NO per-call state stranded: it is made at
/// router ingress, before the dispatch offer, so no per-call queue, worker or
/// lock is ever allocated for the refused call_ref. After the refusal the
/// worker is fully reaped (`creations == removals`, `lock_count == 0`, no live
/// call).
#[tokio::test]
async fn a_shed_strands_no_per_call_lock() {
    let h = Harness::with_transit_delay("b2bua-router-admission-shed-no-lock-leak", 0)
        .describe("a bucket refusal leaves no per-call queue or lock behind");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let _bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5076)
        .tune(|c| {
            c.cps_bucket_size = 0;
            c.cps_bucket_rate = 0;
        })
        .start(&h, "b2bua", "127.0.0.1:5086")
        .await;

    // First non-emergency INVITE is shed deterministically (empty bucket).
    let mut call = alice.invite(&_bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    call.expect(503).await;

    // The refusal is counted once; no per-call queue was opened for it.
    settle_until(|| b2bua.new_calls().rejected(Refusal::BucketEmpty, Class::Normal) == 1).await;
    settle_until(|| b2bua.is_reaped()).await;

    // The strongest oracle: creations == removals, no live call, NO stranded lock,
    // no stamp residue. A refusal path that merely `return`ed would fail this on
    // `lock_count`/creations.
    b2bua.assert_fully_reaped();

    let _r = h.finish().await;
}

/// An **emergency** INVITE (carrying an emergency Resource-Priority) bypasses the
/// empty bucket: it is admitted, routed to bob, and the call establishes — even
/// though a non-emergency INVITE against the same worker would be shed. Proves the
/// emergency bypass (`isEmergency → always admit`) in the live path.
#[tokio::test]
async fn emergency_invite_bypasses_the_empty_bucket_and_establishes() {
    let h = Harness::with_transit_delay("b2bua-router-admission-emergency-bypass", 0)
        .describe("an emergency INVITE is admitted past an exhausted CPS bucket");
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5074)
        .tune(|c| {
            c.cps_bucket_size = 0;
            c.cps_bucket_rate = 0;
        })
        .start(&h, "b2bua", "127.0.0.1:5084")
        .await;

    // `Resource-Priority: esnet.0` marks the INVITE emergency (is_emergency_request).
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Resource-Priority", "esnet.0")
        .through(b2bua.addr)
        .send()
        .await;

    // Admitted despite the empty bucket: the B2BUA bridges to bob.
    let mut uas = bob.receive("INVITE").await;
    assert!(!uas.request().body().is_empty(), "offer relayed to bob on the emergency call");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert!(!ok.body().is_empty(), "answer relayed back to alice");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Emergency admits are NOT counted on `adm` (the LB caps non-emergency only).
    assert_eq!(
        b2bua.overload().metrics().non_emergency_admitted_total,
        0,
        "emergency admits must not advance the adm counter"
    );
    assert_eq!(b2bua.new_calls().rejected_sum(), 0, "an emergency call is never shed by the gate");

    // Teardown so the harness ends clean.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

/// A run of emergency calls past an exhausted bucket leaves no debt: the next
/// non-emergency INVITE is refused with a Retry-After of one refill interval,
/// and is admitted and established once that interval has passed. Paused clock,
/// rate 1/s: five emergency calls with a debt would hold new calls off for 5 s,
/// and no token can refill while the calls run.
#[tokio::test(start_paused = true)]
async fn non_emergency_admission_resumes_one_refill_after_an_emergency_run() {
    let h = Harness::with_transit_delay("b2bua-router-admission-emergency-no-debt", 0)
        .describe("emergency calls past an empty bucket leave no debt for the next call");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5077)
        .tune(|c| {
            c.cps_bucket_size = 1;
            c.cps_bucket_rate = 1;
            // Unjittered, so the 503 states the time-to-token itself.
            c.retry_after_base_sec = 1;
            c.retry_after_jitter_sec = 0;
        })
        .start(&h, "b2bua", "127.0.0.1:5087")
        .await;

    // Five emergency calls: the first spends the lone token, the rest pass the
    // empty bucket.
    for _ in 0..5 {
        let mut call = alice
            .invite(&bob)
            .with_sdp(OFFER)
            .with_header("Resource-Priority", "esnet.0")
            .through(b2bua.addr)
            .send()
            .await;
        bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        bob.receive("ACK").await;
        let mut bye = dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
    }

    // The bucket is empty, not in debt: one token away at 1/s.
    let mut shed = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let resp = shed.expect(503).await;
    let retry = resp.header::<RetryAfter>().expect("a Retry-After").expect("readable Retry-After");
    assert_eq!(retry.token(), "1", "Retry-After is one refill interval");

    h.advance(std::time::Duration::from_secs(1)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(b2bua.new_calls().accepted(Class::Emergency), 5);
    assert_eq!(b2bua.overload().metrics().non_emergency_admitted_total, 1);
    assert_eq!(b2bua.new_calls().rejected(Refusal::BucketEmpty, Class::Normal), 1);
    settle_until(|| b2bua.cdr_records().len() == 6).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

/// A non-emergency new INVITE admitted by a bucket with room advances the
/// published `adm` counter (the X-Overload `adm` field LBs diff for treated rate)
/// exactly once, and the call proceeds normally to bob. Port of the TS
/// `incrementNonEmergencyAdmitted`-on-admit contract, end-to-end.
#[tokio::test]
async fn admitted_non_emergency_invite_advances_the_adm_counter() {
    let h = Harness::with_transit_delay("b2bua-router-admission-admit-counts", 0)
        .describe("an admitted non-emergency INVITE advances the published adm counter");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    // A roomy bucket (the default 1000/500 would also do; explicit for clarity).
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5075)
        .tune(|c| {
            c.cps_bucket_size = 10;
            c.cps_bucket_rate = 10;
        })
        .start(&h, "b2bua", "127.0.0.1:5085")
        .await;

    assert_eq!(b2bua.overload().metrics().non_emergency_admitted_total, 0, "no admit yet");

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // The admit advanced `adm` exactly once and shed nothing.
    assert_eq!(
        b2bua.overload().metrics().non_emergency_admitted_total,
        1,
        "an admitted non-emergency new dialog advances adm by 1"
    );
    assert_eq!(b2bua.new_calls().rejected_sum(), 0);
    // The published header reflects it.
    assert!(
        b2bua.overload().x_overload_header_value().ends_with("adm=1"),
        "the X-Overload header publishes adm=1 after the admit, got {:?}",
        b2bua.overload().x_overload_header_value()
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

/// Every new INVITE an empty bucket refuses gets the new-call 503's jittered
/// `Retry-After`, from a base no sooner than a token exists: INVITEs refused
/// at one instant do not all ask to come back at once. A bucket that never
/// refills is a minute from its next token; base 5 s, jitter 10 s.
#[tokio::test(start_paused = true)]
async fn invites_refused_by_an_empty_bucket_get_a_jittered_retry_after() {
    let h = Harness::with_transit_delay("b2bua-router-admission-jittered-retry-after", 0)
        .describe("an empty bucket's 503s spread their Retry-After past the time-to-token");
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5078)
        .tune(|c| {
            c.cps_bucket_size = 0;
            c.cps_bucket_rate = 0;
            c.retry_after_base_sec = 5;
            c.retry_after_jitter_sec = 10;
        })
        .start(&h, "b2bua", "127.0.0.1:5088")
        .await;

    let mut calls = Vec::new();
    for _ in 0..8 {
        calls.push(alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await);
    }
    let mut values = Vec::new();
    for call in &mut calls {
        let resp = call.expect(503).await;
        let retry = resp.header::<RetryAfter>().expect("a Retry-After").expect("readable");
        values.push(retry.token().parse::<u32>().expect("numeric Retry-After"));
    }
    assert!(
        values.iter().all(|v| (60..=70).contains(v)),
        "time-to-token 60 s, jitter 10 s: {values:?}"
    );
    assert!(values.iter().any(|v| *v != values[0]), "the refusals are spread: {values:?}");

    settle_until(|| b2bua.new_calls().rejected(Refusal::BucketEmpty, Class::Normal) == 8).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

/// A panic-ELU refusal spends no CPS token: the backstop is judged before the
/// bucket, and a token is spent only on an admit. One token that never
/// refills: an INVITE refused above the panic threshold leaves it in the
/// bucket, and the next INVITE, once the loop is back below the threshold,
/// takes it and is established.
#[tokio::test(start_paused = true)]
async fn a_panic_elu_refusal_spends_no_cps_token() {
    let (sampler, load) = load_shed::simulated();
    let h = Harness::with_transit_delay("b2bua-router-admission-panic-spends-no-token", 0)
        .describe("a panic-ELU refusal leaves the CPS token count unchanged");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071)
        .overload(b2bua::overload::OverloadSignal::new(std::sync::Arc::new(sampler)))
        .tune(|c| {
            c.cps_bucket_size = 1;
            c.cps_bucket_rate = 0;
            c.overload_panic_elu_threshold = 0.5;
        })
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;
    let overload = b2bua.overload().clone();

    load.set_elu(0.9);
    settle_until(|| overload.metrics().elu_ewma > 0.5).await;
    let mut refused = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    refused.expect(503).await;
    assert_eq!(
        overload.metrics().token_bucket_level,
        1.0,
        "a panic-ELU refusal leaves the token in the bucket"
    );

    load.set_elu(0.0);
    settle_until(|| overload.metrics().elu_ewma < 0.5).await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}
