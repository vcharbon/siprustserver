//! What the releasing peer said travels onto the teardown minted for the other
//! leg (RFC 3261 §16.6, RFC 3326 §2).
//!
//! A back-to-back UA answers the BYE itself and mints a fresh one toward the
//! peer, and cancels the pending INVITE with a CANCEL of its own. Both are the
//! stack's own messages, so nothing the releasing peer stated reaches the far
//! side unless these mint points carry it — and `Reason` is the Q.850 cause a
//! PSTN gateway and both CDRs are built on.

use b2bua_harness::{settle_until, B2buaScene};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipRequest;

fn stated(req: &SipRequest, name: &str) -> Option<String> {
    req.raw_text(HeaderName::from(name)).next().map(|v| v.as_str().to_string())
}

/// Alice releases with a cause, a charging correlation, a vendor annotation and
/// end-to-end user data; the BYE the SUT mints toward bob restates all four.
#[tokio::test(start_paused = true)]
async fn a_releasing_peers_headers_ride_the_bye_minted_for_the_other_leg() {
    let s = B2buaScene::new("b2bua-teardown-header-relay").await;
    let mut dialog = s.establish().await;

    let mut bye = dialog
        .send_request(InDialogMethod::Bye)
        .with_header("Reason", "Q.850;cause=16")
        .with_header("P-Charging-Vector", "icid-value=\"alice-icid-1\"")
        .with_header("P-Vendor-Thing", "annotation")
        .with_header("User-to-User", "3132333435;encoding=hex")
        .send()
        .await;

    let mut bob_uas = s.bob.receive("BYE").await;
    {
        let req = bob_uas.request();
        assert_eq!(stated(req, "Reason").as_deref(), Some("Q.850;cause=16"));
        assert_eq!(
            stated(req, "P-Charging-Vector").as_deref(),
            Some("icid-value=\"alice-icid-1\""),
            "the charging correlation survives the release"
        );
        assert_eq!(stated(req, "P-Vendor-Thing").as_deref(), Some("annotation"));
        assert_eq!(
            stated(req, "User-to-User").as_deref(),
            Some("3132333435;encoding=hex"),
            "RFC 7433 data addressed to the far endpoint reaches it"
        );
        assert_ne!(
            stated(req, "Call-ID").as_deref(),
            Some("alice-call-id"),
            "the minted BYE still owns this leg's dialog identity"
        );
    }
    bob_uas.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// The CANCEL twin: the transaction layer answers alice's CANCEL, so the cause
/// she gave reaches bob only if the CANCEL minted toward him restates it.
#[tokio::test(start_paused = true)]
async fn a_cancellers_release_cause_rides_the_cancel_minted_for_the_callee() {
    let s = B2buaScene::new("b2bua-cancel-header-relay").await;

    let mut call = s.alice.invite(&s.bob).through(s.b2bua.addr).send().await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel_stating(&[("Reason", "Q.850;cause=16")]).await;
    cxl.expect(200).await;

    let mut bob_cxl = s.bob.receive("CANCEL").await;
    assert_eq!(
        stated(bob_cxl.request(), "Reason").as_deref(),
        Some("Q.850;cause=16"),
        "the canceller's cause reaches the callee"
    );
    bob_cxl.respond(200, "OK").await;
    bob_uas.respond(487, "Request Terminated").await;
    call.expect(487).await;

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// RFC 3261 §13.3.1.4 makes a retransmit a COPY of the 2xx it retransmits, so
/// the advertisement it carries is the one that 2xx carried — the callee's own,
/// relayed — and not whatever the face would resolve to a second time.
#[tokio::test(start_paused = true)]
async fn a_retransmitted_2xx_repeats_the_advertisement_the_first_one_stated() {
    use std::time::Duration;

    let s = B2buaScene::new("b2bua-2xx-retransmit-advert").await;
    let mut call = s.alice.invite(&s.bob).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    // The callee states a set of its own; alice then stays silent, so the SUT
    // retransmits its answer. The harness inbox dedups a retransmit, so the
    // copies are read off the recorded wire rather than received.
    uas.respond(200, "OK")
        .with_header("Allow", "INVITE, ACK, BYE")
        .with_header("Supported", "path, sdp-anat")
        .await;
    call.expect(200).await;
    for _ in 0..3 {
        s.h.advance(Duration::from_millis(600)).await;
    }
    s.alice.drain().await;

    // Alice ACKs late; the call then ends normally.
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    let sut = s.b2bua.addr;
    let report = s.finish().await;

    let answers: Vec<String> = report
        .entries()
        .iter()
        .filter(|e| e.from == sut)
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .filter(|text| text.starts_with("SIP/2.0 200") && text.contains("CSeq: 1 INVITE"))
        .collect();
    assert!(answers.len() >= 2, "alice's silence must produce a retransmit, got {}", answers.len());
    let advert = |text: &str| -> Vec<String> {
        text.split("\r\n")
            .filter(|line| {
                let lower = line.to_ascii_lowercase();
                lower.starts_with("allow:") || lower.starts_with("supported:")
            })
            .map(str::to_string)
            .collect()
    };
    let first = advert(&answers[0]);
    assert!(
        first.iter().any(|l| l.contains("path")),
        "the answer relayed the callee\'s own option tags: {first:?}"
    );
    for copy in &answers[1..] {
        assert_eq!(advert(copy), first, "a retransmit restates the answer it repeats");
    }
}
