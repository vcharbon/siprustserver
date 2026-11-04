//! A b-leg INVITE whose next hop is not an IP literal fails at once: the
//! worker resolves no name, so with no outbound proxy configured an
//! allow-listed name destination cannot be put on the wire, and the call takes
//! its dead-hop response-timeout path at once instead of waiting on a B2BUA
//! timer.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};

/// Admitted by the default allow-list (`.svc.cluster.local`), never resolved.
const NAMED_CALLEE: &str = "callee.ns.svc.cluster.local";

/// alice is answered 480 in the turn right after the INVITE's; nothing
/// reaches bob. The
/// CANCELed Trying b-leg stays unresolved, as on a Timer B give-up, so the
/// call is reaped at its terminating safety timeout (32 s).
#[tokio::test(start_paused = true)]
async fn a_b_leg_invite_to_a_name_fails_the_attempt_at_once() {
    let s = B2buaScene::with_b2bua("unaddressable-next-hop", |bob_port| {
        B2buaSut::route_all_to(NAMED_CALLEE, bob_port)
    })
    .await;
    let started = tokio::time::Instant::now();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    call.expect(480).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "an unaddressable INVITE fails at once, not on a timer: {elapsed:?}"
    );
    assert_eq!(
        s.b2bua.metrics().outbound_unaddressable_of("request"),
        2,
        "the INVITE and the CANCEL that abandons its leg are counted unsent"
    );

    s.h.advance(Duration::from_secs(33)).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    s.finish().await;
}

/// The caller's Contact names a host, not an IP literal; the callee hangs up.
/// The BYE toward the caller cannot leave: it is logged with its target and
/// counted, the caller's dialog ends as on a BYE timeout (RFC 3261 §15), and
/// the call writes its CDR and is reaped. The caller's own BYE then finds no
/// dialog and is answered 481 (§12.2.2).
#[tokio::test(start_paused = true)]
async fn an_in_dialog_bye_to_a_named_target_is_logged_and_the_call_still_ends() {
    let (_log_guard, log) = observe_testkit::test_buffer();
    let s = B2buaScene::new("unaddressable-in-dialog-target").await;
    let target = "sip:alice@alice.invalid:5060";
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_contact(target)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    bob_dialog.bye().await.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    assert_eq!(s.b2bua.metrics().outbound_unaddressable_of("request"), 1, "the BYE is unsent");
    let unsent = log.matching("not sent");
    assert!(
        unsent.iter().filter(|e| e.level == "WARN" && e.contains(target)).count() == 1,
        "one warning names the BYE's target: {:?}",
        unsent.iter().map(|e| e.line()).collect::<Vec<_>>()
    );

    alice_dialog.bye().await.expect(481).await;
    s.finish().await;
}
