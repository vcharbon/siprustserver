//! Tier-3 admission gate (migration/09 — port of `OverloadController.shouldAdmit`
//! + the stateless-503 gate the TS `TransactionLayer` runs on a new INVITE).
//!
//! End-to-end through a running `B2buaCore`: a new-dialog INVITE is gated by the
//! CPS token bucket + the panic-ELU backstop before any call/dialog state is
//! created. The gate's UNIT behaviour (bucket drain/refill, panic-ELU, emergency
//! overdraft, reason tags) is pinned in `b2bua::overload::tests`; this file proves
//! the WIRING — the verdict turns into a real stateless 503 on the wire (with the
//! overload `Reason` + `Retry-After`), no per-call resources are born for a reject,
//! emergency bypasses the empty bucket, and an admit advances the published `adm`.
//!
//! Real-clock (not `start_paused`): every assertion is instant — a size-0 bucket
//! rejects/admits the FIRST INVITE with no timer to wait on, so the suite stays
//! far under the 60 s slow-lane threshold. The time-based refill path is the one
//! piece that needs a clock, and it is covered by the paused-clock unit test
//! `overload::tests::the_bucket_refills_over_time`.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A non-emergency new INVITE against a worker whose CPS bucket is exhausted
/// (capacity 0, no refill) is shed with a **stateless 503**: the caller gets the
/// `503` carrying `Reason: …text="overload"` + a `Retry-After`, and NO per-call
/// state is created — no CDR, no live call, and bob is never contacted.
#[tokio::test]
async fn cps_bucket_empty_503s_a_new_invite_statelessly() {
    let h = Harness::with_transit_delay("b2bua-tier3-bucket-empty", 0)
        .describe("an exhausted CPS bucket sheds a new INVITE with a stateless overload 503");
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

    // The gate rejects: the caller's INVITE gets a 503 (after the txn layer's
    // absorbed auto-100). It carries the overload cause + a Retry-After, and a
    // To-tag (this codebase tags every non-100 final; the ACK is still absorbed
    // by the rejecting INVITE server txn — stateless at the call layer).
    let resp = call.expect(503).await;
    let reason = get_header(&resp.headers, "reason").unwrap_or("");
    assert!(
        reason.contains("text=\"overload\""),
        "503 Reason must mark the overload cause, got {reason:?}"
    );
    assert!(
        get_header(&resp.headers, "retry-after").is_some(),
        "overload 503 must carry a Retry-After hint"
    );
    assert!(resp.to.tag.is_some(), "non-100 final carries a To-tag (RFC §8.2.6.2)");

    // No per-call state was created for the rejected INVITE: no live call, and the
    // worker counted the shed on its overload-reject metric.
    settle_until(|| b2bua.metrics().overload_rejected_total() == 1).await;
    assert_eq!(b2bua.active_calls(), 0, "a rejected INVITE creates no live call");
    assert!(
        b2bua.cdr_records().is_empty(),
        "a stateless reject writes no CDR (no call was ever born)"
    );
    // The reject is NOT counted on the published `adm` (only admitted
    // non-emergency new dialogs are).
    assert_eq!(b2bua.overload().metrics().non_emergency_admitted_total, 0);

    let _r = h.finish().await;
}

/// An **emergency** INVITE (carrying an emergency Resource-Priority) bypasses the
/// empty bucket: it is admitted, routed to bob, and the call establishes — even
/// though a non-emergency INVITE against the same worker would be shed. Proves the
/// emergency bypass (`isEmergency → always admit`) in the live path.
#[tokio::test]
async fn emergency_invite_bypasses_the_empty_bucket_and_establishes() {
    let h = Harness::with_transit_delay("b2bua-tier3-emergency-bypass", 0)
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
    assert!(!uas.request().body.is_empty(), "offer relayed to bob on the emergency call");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "answer relayed back to alice");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Emergency admits are NOT counted on `adm` (the LB caps non-emergency only).
    assert_eq!(
        b2bua.overload().metrics().non_emergency_admitted_total, 0,
        "emergency admits must not advance the adm counter"
    );
    assert_eq!(
        b2bua.metrics().overload_rejected_total(),
        0,
        "an emergency call is never shed by the gate"
    );

    // Teardown so the harness ends clean.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    let _r = h.finish().await;
}

/// A non-emergency new INVITE admitted by a bucket with room advances the
/// published `adm` counter (the X-Overload `adm` field LBs diff for treated rate)
/// exactly once, and the call proceeds normally to bob. Port of the TS
/// `incrementNonEmergencyAdmitted`-on-admit contract, end-to-end.
#[tokio::test]
async fn admitted_non_emergency_invite_advances_the_adm_counter() {
    let h = Harness::with_transit_delay("b2bua-tier3-admit-counts", 0)
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

    assert_eq!(
        b2bua.overload().metrics().non_emergency_admitted_total, 0,
        "no admit yet"
    );

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // The admit advanced `adm` exactly once and shed nothing.
    assert_eq!(
        b2bua.overload().metrics().non_emergency_admitted_total, 1,
        "an admitted non-emergency new dialog advances adm by 1"
    );
    assert_eq!(b2bua.metrics().overload_rejected_total(), 0);
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

    let _r = h.finish().await;
}
