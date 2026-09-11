//! A `481` answering a PRACK denies ONE TRANSACTION, not the dialog, so the
//! call it belongs to survives it (issue 264 item A).
//!
//! RFC 3262 §3 makes `481 Call/Transaction Does Not Exist` the ORDINARY answer
//! to a PRACK matching no unacknowledged reliable provisional — the responder
//! has already acknowledged it, or never held it. RFC 3261 §12.2.1.2's
//! terminate-the-dialog reading applies to a 481 that denies the DIALOG (a
//! re-INVITE, UPDATE, INFO, BYE, MESSAGE, or the keepalive OPTIONS); a PRACK's
//! 481 denies a single transaction, and §14.1 leaves the dialog in its prior
//! state.
//!
//! This stack PRACKs on the responder's behalf wherever the originator never
//! offered `100rel`, so the PRACK is its own — it leaves no relayed pending
//! request, and the 481 answering it must still not tear the call down.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::B2buaSut;
use scenario_harness::run::RunReport;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// Finish the run and render its SIP call-flow artifacts (`<name>.html` +
/// `.svg` + `.global.txt`) under `target/seq-reports/prack-remainder/`, so the
/// ladder each assertion below reads has a sequence diagram beside it.
fn write_flow_report(report: &RunReport) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports/prack-remainder");
    let paths = scenario_harness::report::write_all(report, &dir).expect("write report");
    if let Some(html) = paths.iter().find(|p| p.extension().is_some_and(|e| e == "html")) {
        eprintln!("prack-remainder report: {}", html.display());
    }
}

/// Bob's own sequence — far from anything this stack mints first.
const BOB_RSEQ: u32 = 4711;

/// A decision that DECLARES `Supported: 100rel` toward the originated face, so
/// the callee answers reliably although the caller offered nothing — the shape
/// that makes the PRACK this stack's own.
fn decision_offering_100rel_toward_bob(port: u16) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", port);
                r.features.advertise_capabilities =
                    Some(call::features::AdvertiseCapabilitiesFeature {
                        toward_originator: None,
                        toward_originated: Some(call::features::AdvertisedCapabilities {
                            allow: None,
                            supported: Some(vec!["100rel".to_string()]),
                        }),
                    });
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Bob answers this stack's PRACK `481`: by the time it reached him he no
/// longer held that provisional unacknowledged (RFC 3262 §3). The re-INVITE
/// completes, and the established call is untouched — no BYE, no teardown.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   re-INVITE(no 100rel) → [b2bua: Supported:100rel] → 183(100rel,RSeq 4711)
///          ← 183(plain) ; b2bua PRACK → 481(PRACK)      [§3's ordinary answer]
///          → 200(INVITE) → ACK → BYE → 200(BYE)          [the call lives on]
/// ```
#[tokio::test]
async fn a_481_answering_a_stack_originated_prack_does_not_end_the_call() {
    let h = Harness::with_transit_delay("b2bua-prack-481-in-dialog", 0);
    // On the wire bob's 481 contradicts the reliable 183 he himself sent, so the
    // audit reads it as a §3 breach — which is exactly the fixture: a responder
    // that no longer holds that provisional (its state lost, or already
    // acknowledged) owes §3's 481, and the wire cannot see the difference.
    // Waived on bob alone, so every B2BUA bind stays gated.
    for rule in ["prack-2xx-or-481", "prack-accepted-after-final"] {
        h.waive(
            WaiverScope::rule(
                rule,
                "bob deliberately answers the stack's PRACK 481 (RFC 3262 §3's answer for a \
                 provisional he no longer holds unacknowledged) — surviving that 481 is what \
                 this test measures",
            )
            .on_party("bob"),
        );
    }
    let alice = h.agent("alice", "127.0.0.1:5177").await;
    let bob = h.agent("bob", "127.0.0.1:5178").await;
    let b2bua = B2buaSut::builder(decision_offering_100rel_toward_bob(5178))
        .start(&h, "b2bua", "127.0.0.1:5179")
        .await;

    // ── an ordinary call, established without reliability in play ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs, offering NO reliable provisionals ──
    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .with_header("Supported", "timer")
        .send()
        .await;
    let mut re_uas = bob.receive("INVITE").await;

    // Bob answers reliably; alice is shown the ordinary copy; this stack PRACKs.
    re_uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(REANSWER).await;
    reinv.expect(183).await;

    // §3's ordinary answer: bob no longer holds that provisional unacknowledged.
    bob.receive("PRACK").await.respond(481, "Call/Transaction Does Not Exist").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The 481 denied one transaction. Neither party is being torn down.
    assert!(
        alice.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a PRACK's 481 denies one transaction (RFC 3262 §3), so the caller's established dialog \
         stands — RFC 3261 §12.2.1.2's terminate reading is for a 481 that denies the DIALOG",
    );
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "the callee's dialog stands too — nothing in the 481 denied it",
    );

    // ── bob answers the re-INVITE; alice ACKs the 2xx ──
    re_uas.respond(200, "OK").with_sdp(REANSWER).await;
    let final_200 = reinv.expect(200).await;
    alice_dialog.ack_for(final_200.cseq().seq(), None).await;
    bob.receive("ACK").await;

    // ── teardown, on the caller's own clock ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    write_flow_report(&report);
    b2bua.assert_fully_reaped();
}
