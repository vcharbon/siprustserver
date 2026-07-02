//! The generic **fallible any-method** surface + the RFC 3262/3311 choreography,
//! peer-to-peer (no SUT): every step uses the `try_*` lane (a deviation is a
//! `StepError`, never a panic), and the run must pass the mandatory RFC
//! 3261/3262/3264 audit at `finish()` with NO `allow_violation`.

use scenario_harness::{Harness, StepError};
use sip_message::generators::{InDialogMethod, OutOfDialogMethod};
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER_A: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const REANSWER_B: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const REOFFER_B: &str = "v=0\r\no=bob 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";
const REANSWER_A: &str = "v=0\r\no=alice 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";

/// Full 100rel establishment + UPDATE in BOTH directions, all on the fallible
/// surface: INVITE(Supported:100rel) → reliable 183(RSeq) → PRACK(RAck) →
/// 200(PRACK) → 200(INVITE) → ACK → UPDATE(A→B) → 200 → UPDATE(B→A) → 200 →
/// BYE. The RAck is derived from the 183 itself (`try_prack`); the whole trace
/// is gated by the RFC audit — no waivers.
#[tokio::test(start_paused = true)]
async fn prack_then_update_both_directions_fallible() -> Result<(), StepError> {
    let h = Harness::new("any-method-prack-update");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // Alice opts into 100rel on the INVITE (RFC 3262 §3).
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .send()
        .await;

    // Bob answers RELIABLY: 183 + Require:100rel + RSeq (the `reliable` sugar).
    let mut uas = bob.try_receive("INVITE").await?;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).try_send().await?;
    let p183 = call.try_expect(183).await?;
    assert_eq!(get_header(&p183.headers, "rseq").as_deref(), Some("1"));

    // Alice PRACKs it — RAck (`<RSeq> <CSeq> INVITE`) derived from the 183.
    let mut prack = call.try_prack(&p183).await?;
    let mut prack_uas = bob.try_receive("PRACK").await?;
    assert_eq!(
        get_header(&prack_uas.request().headers, "rack").as_deref(),
        Some("1 1 INVITE"),
        "RAck must reference the 183's RSeq + the INVITE's CSeq (RFC 3262 §7.2)",
    );
    prack_uas.respond(200, "OK").try_send().await?;
    prack.try_expect(200).await?;

    // Only now (RFC 3262 MUST-014) does bob answer; alice ACKs.
    uas.respond(200, "OK").with_sdp(ANSWER).try_send().await?;
    call.try_expect(200).await?;
    let mut dialog = call.ack().await;
    bob.try_receive("ACK").await?;

    // UPDATE alice → bob (RFC 3311): re-offer in the UPDATE, answer in its 200.
    let mut upd_a = dialog
        .send_request(InDialogMethod::Update)
        .with_sdp(REOFFER_A)
        .try_send()
        .await?;
    let mut upd_uas = bob.try_receive("UPDATE").await?;
    assert!(upd_uas.request().body.starts_with(b"v=0"), "UPDATE carries the re-offer");
    upd_uas.respond(200, "OK").with_sdp(REANSWER_B).try_send().await?;
    upd_a.try_expect(200).await?;

    // UPDATE bob → alice — the same generic surface from the UAS-side dialog.
    let mut bob_dialog = uas.dialog();
    let mut upd_b = bob_dialog
        .send_request(InDialogMethod::Update)
        .with_sdp(REOFFER_B)
        .try_send()
        .await?;
    let mut upd_a_uas = alice.try_receive("UPDATE").await?;
    upd_a_uas.respond(200, "OK").with_sdp(REANSWER_A).try_send().await?;
    upd_b.try_expect(200).await?;

    // Teardown.
    let mut bye = dialog.bye().await;
    bob.try_receive("BYE").await?.respond(200, "OK").try_send().await?;
    bye.try_expect(200).await?;

    // Mandatory RFC 3261/3262/3264 hard gate — must pass with NO waiver.
    let _report = h.finish().await;
    Ok(())
}

/// The generic out-of-dialog builder ([`Agent::request`]) auto-fills the
/// mechanical layer for ANY `OutOfDialogMethod`: a MESSAGE with a caller-supplied
/// body/Content-Type/custom header round-trips, fallibly.
#[tokio::test(start_paused = true)]
async fn generic_out_of_dialog_message_fallible() -> Result<(), StepError> {
    let h = Harness::new("any-method-out-of-dialog");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut msg = alice
        .request(OutOfDialogMethod::Message, &bob)
        .with_body("text/plain", "hello via the generic surface")
        .with_header("Subject", "any-method")
        .try_send()
        .await?;

    let mut uas = bob.try_receive("MESSAGE").await?;
    let req = uas.request();
    assert_eq!(req.cseq.method, "MESSAGE", "CSeq method auto-filled");
    assert_eq!(get_header(&req.headers, "subject").as_deref(), Some("any-method"));
    assert_eq!(
        get_header(&req.headers, "content-type").as_deref(),
        Some("text/plain"),
        "caller-supplied Content-Type is used",
    );
    assert_eq!(req.body, b"hello via the generic surface");
    uas.respond(200, "OK").try_send().await?;
    msg.try_expect(200).await?;

    let _report = h.finish().await;
    Ok(())
}

/// Deviations on the new surface surface as [`StepError`] values — never a
/// panic: a wrong final status classifies as `WrongStatus`, and a receive with
/// nothing inbound as `Timeout`.
#[tokio::test(start_paused = true)]
async fn generic_surface_errors_are_step_errors() {
    let h = Harness::new("any-method-errors");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // Nothing was sent to bob → a fallible receive times out as a StepError.
    match bob.try_receive("UPDATE").await {
        Err(StepError::Timeout { who }) => assert_eq!(who, "bob"),
        Err(other) => panic!("expected Timeout, got {other:?}"),
        Ok(_) => panic!("expected Timeout, got a request"),
    }

    // Bob answers an OPTIONS 200; expecting a 486 classifies as WrongStatus.
    let mut opts = alice
        .request(OutOfDialogMethod::Options, &bob)
        .with_header("Accept", "application/sdp")
        .try_send()
        .await
        .unwrap();
    let mut uas = bob.try_receive("OPTIONS").await.unwrap();
    uas.respond(200, "OK").try_send().await.unwrap();
    match opts.try_expect(486).await {
        Err(StepError::WrongStatus { expected: 486, got: 200, .. }) => {}
        other => panic!("expected WrongStatus 486/200, got {other:?}"),
    }

    let _report = h.finish().await;
}
