//! The `Reason` (RFC 3326) on the teardowns the stack mints, under the
//! deployment's statements: the CANCEL toward a pending callee restating the
//! canceller's cause, and the BYEs of a release the stack makes on its own.
//! No `Reason` reaches a media leg: it is the dialling service's resource, not
//! a party the cause informs.
//!
//! RFC 3326 §2 makes `Reason` the sender's own statement and a MAY on every
//! request, so both are deployment choices; the defaults (verbatim relay, no
//! own cause) are pinned by `teardown_header_relay` and `early_dialog_bye`.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use b2bua_sdk::release_reason::CancelReason;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The cause a release of the stack's own states in these scenarios.
const OWN_CAUSE: &str = "Q.850;cause=16";

/// Every `Reason` line `req` carries, as written.
fn reasons(req: &SipRequest) -> Vec<String> {
    req.raw_text(HeaderName::Reason).map(|v| v.as_str().to_string()).collect()
}

/// A paused-clock harness with alice, bob and a SUT routing every call to bob
/// under `tune`, and `max_duration_sec` capping the call (0: the route's own cap).
async fn scene(
    name: &str,
    port: u16,
    max_duration_sec: i64,
    tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, scenario_harness::Agent, scenario_harness::Agent, B2buaSut) {
    let h = Harness::new(name);
    let alice = h.agent("alice", &format!("127.0.0.1:{port}")).await;
    let bob_port = port + 10;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut route = route_to("127.0.0.1", bob_port);
                if max_duration_sec > 0 {
                    route.features.platform.max_duration_sec = max_duration_sec;
                }
                NewCallResponse::Route(route)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(tune)
        .start(&h, "b2bua", &format!("127.0.0.1:{}", port + 20))
        .await;
    (h, alice, bob, b2bua)
}

/// The caller's CANCEL states a Q.850 value with text and location, then a
/// SIP value: the CANCEL toward bob carries the Q.850 cause alone, its digits
/// as written.
#[tokio::test(start_paused = true)]
async fn a_relayed_cancel_restates_a_leading_q850_cause_alone() {
    cancel_restated(
        "b2bua-cancel-q850-alone",
        5161,
        &["Q.850;cause=031;text=\"Normal\";Location=A", "SIP;cause=487;text=\"ORIGINATOR_CANCEL\""],
        &["Q.850;cause=031"],
    )
    .await;
}

/// Only a LEADING Q.850 value is restated: a CANCEL whose first value is a
/// SIP one carries no `Reason` toward bob, its Q.850 value behind it included.
#[tokio::test(start_paused = true)]
async fn a_cancel_whose_first_value_is_not_q850_relays_no_reason_under_the_restatement() {
    cancel_restated(
        "b2bua-cancel-sip-first",
        5162,
        &["SIP;cause=487;text=\"ORIGINATOR_CANCEL\"", "Q.850;cause=16"],
        &[],
    )
    .await;
}

/// A CANCEL stating SIP values only has no Q.850 cause to restate.
#[tokio::test(start_paused = true)]
async fn a_cancel_stating_sip_values_only_relays_no_reason_under_the_restatement() {
    cancel_restated("b2bua-cancel-sip-only", 5163, &["SIP;cause=480;text=\"NO_ANSWER\""], &[])
        .await;
}

/// alice CANCELs a ringing call stating `stated`; under the Q.850
/// restatement the CANCEL toward bob carries `expected`.
async fn cancel_restated(name: &str, port: u16, stated: &[&str], expected: &[&str]) {
    let (h, alice, bob, b2bua) = scene(name, port, 0, |c| {
        c.relayed_cancel_reason = CancelReason::Q850CauseAlone;
    })
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let lines: Vec<(&str, &str)> = stated.iter().map(|r| ("Reason", *r)).collect();
    let mut cxl = call.cancel_stating(&lines).await;
    cxl.expect(200).await;

    let mut bob_cxl = bob.receive("CANCEL").await;
    assert_eq!(reasons(bob_cxl.request()), expected, "the CANCEL toward bob");
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    call.expect(487).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The CANCEL minted for the caller's early BYE (RFC 3261 §15) restates its
/// Q.850 cause the same way.
#[tokio::test(start_paused = true)]
async fn the_cancel_minted_for_an_early_bye_restates_the_q850_cause_alone() {
    let (h, alice, bob, b2bua) = scene("b2bua-early-bye-q850-alone", 5164, 0, |c| {
        c.relayed_cancel_reason = CancelReason::Q850CauseAlone;
    })
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut bye = call
        .send_request(InDialogMethod::Bye)
        .with_header("Reason", "Q.850;cause=16;text=\"Normal call clearing\"")
        .send()
        .await;
    bye.expect(200).await;
    call.expect(487).await;

    let mut bob_cxl = bob.receive("CANCEL").await;
    assert_eq!(reasons(bob_cxl.request()), ["Q.850;cause=16"]);
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// Establish alice ↔ bob through the SUT and return alice's dialog.
async fn establish(
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    b2bua: &B2buaSut,
) -> scenario_harness::Dialog {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    dialog
}

/// The call's duration cap ends it: the stack's own release states the
/// deployment's cause on the BYE toward each party.
#[tokio::test(start_paused = true)]
async fn an_own_release_states_the_deployments_cause_toward_both_parties() {
    let (h, alice, bob, b2bua) = scene("b2bua-own-release-cause", 5165, 20, |c| {
        c.own_release_reason = Some(OWN_CAUSE.into());
    })
    .await;
    let _dialog = establish(&alice, &bob, &b2bua).await;

    h.advance(Duration::from_secs(21)).await;
    let mut to_alice = alice.receive("BYE").await;
    assert_eq!(reasons(to_alice.request()), [OWN_CAUSE], "the caller learns the stack's cause");
    to_alice.respond(200, "OK").await;
    let mut to_bob = bob.receive("BYE").await;
    assert_eq!(reasons(to_bob.request()), [OWN_CAUSE], "the callee learns the stack's cause");
    to_bob.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A release the caller asked for keeps her own words: the BYE minted toward
/// bob relays her `Reason` unchanged, and states none where she stated none,
/// whatever the deployment states for its own releases.
#[tokio::test(start_paused = true)]
async fn a_peer_asked_release_relays_the_peers_reason_and_no_own_cause() {
    for (i, caller_reason) in
        [Some("Q.850;cause=16;text=\"Terminated\""), None].into_iter().enumerate()
    {
        let port = 5166 + i as u16 * 2;
        let (h, alice, bob, b2bua) = scene(&format!("b2bua-peer-release-{i}"), port, 0, |c| {
            c.own_release_reason = Some(OWN_CAUSE.into());
        })
        .await;
        let mut dialog = establish(&alice, &bob, &b2bua).await;

        let mut bye = dialog.send_request(InDialogMethod::Bye);
        if let Some(r) = caller_reason {
            bye = bye.with_header("Reason", r);
        }
        let mut bye = bye.send().await;
        let mut to_bob = bob.receive("BYE").await;
        let expected: Vec<String> = caller_reason.map(str::to_string).into_iter().collect();
        assert_eq!(reasons(to_bob.request()), expected, "the caller's own words, nothing else");
        to_bob.respond(200, "OK").await;
        bye.expect(200).await;

        settle_until(|| b2bua.is_reaped()).await;
        b2bua.assert_fully_reaped();
        let _report = h.finish().await;
    }
}

/// alice CANCELs stating a cause; bob's 200 crosses the CANCEL the SUT sent
/// him, so the SUT ACKs and BYEs him (RFC 3261 §9.1, §13.2.2.4). That BYE is
/// a release of the stack's own: it states the deployment's cause, not the
/// CANCEL's, and none where the deployment states none.
async fn crossing_bye_states(name: &str, port: u16, own: Option<&str>, expected: &[&str]) {
    let own = own.map(str::to_string);
    let (h, alice, bob, b2bua) = scene(name, port, 0, move |c| c.own_release_reason = own).await;
    h.waive(
        scenario_harness::WaiverScope::rule(
            "no-200-after-cancel",
            "bob answers 200 after taking the CANCEL (RFC 3261 §9.2): the crossing under test",
        )
        .on_party("bob"),
    );
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel_stating(&[("Reason", "Q.850;cause=31")]).await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cancel = bob.receive("CANCEL").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob_cancel.respond(200, "OK").await;

    bob.receive("ACK").await;
    let mut bye = bob.receive("BYE").await;
    assert_eq!(reasons(bye.request()), expected, "the BYE that replaces the CANCEL");
    bye.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_bye_replacing_a_crossed_cancel_states_the_own_cause() {
    crossing_bye_states("b2bua-crossed-cancel-own", 5190, Some(OWN_CAUSE), &[OWN_CAUSE]).await;
}

#[tokio::test(start_paused = true)]
async fn the_bye_replacing_a_crossed_cancel_states_none_where_the_deployment_states_none() {
    crossing_bye_states("b2bua-crossed-cancel-none", 5191, None, &[]).await;
}

const MRF_PORT: u16 = 5180;

/// A SUT that plays an announcement from the media server at [`MRF_PORT`]
/// before routing, every Reason option of the deployment armed.
async fn announcement_scene(
    name: &str,
    alice_port: u16,
) -> (Harness, scenario_harness::Agent, scenario_harness::Agent, B2buaSut) {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", MRF_PORT);
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "intro-001",
                        "mrf_host": "127.0.0.1",
                        "mrf_port": MRF_PORT,
                        "dest_host": "127.0.0.1",
                        "dest_port": MRF_PORT,
                        "defer_routing": true,
                    }),
                );
                NewCallResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .services(vec![announcement::service()])
        .tune(|c| {
            c.own_release_reason = Some(OWN_CAUSE.into());
            c.relayed_cancel_reason = CancelReason::Q850CauseAlone;
        })
        .start(&h, "b2bua", &format!("127.0.0.1:{}", alice_port + 1))
        .await;
    (h, alice, mrf, b2bua)
}

/// The media server answers and alice hears the clip; returns her pending
/// call and the media dialog.
async fn hear_the_clip(
    alice: &scenario_harness::Agent,
    mrf: &scenario_harness::Agent,
    b2bua: &B2buaSut,
) -> (scenario_harness::ClientInvite, scenario_harness::Dialog) {
    let mut call = alice.invite(mrf).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(ANSWER).await;
    mrf.receive("ACK").await;
    let mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    (call, mrf_dialog)
}

/// The service's own release (the clip failed after the media server
/// answered) BYEs the media leg with no cause.
#[tokio::test(start_paused = true)]
async fn an_own_release_states_no_cause_toward_a_media_leg() {
    let (h, alice, mrf, b2bua) = announcement_scene("b2bua-own-release-media-leg", 5181).await;
    let (mut call, mut mrf_dialog) = hear_the_clip(&alice, &mrf, &b2bua).await;

    let fail_body = String::from_utf8(announcement::mscml::build_response(480)).unwrap();
    let mut failed_info = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&fail_body)
        .send()
        .await;
    failed_info.expect(200).await;
    call.expect(480).await;

    let mut to_mrf = mrf.receive("BYE").await;
    assert!(reasons(to_mrf.request()).is_empty(), "no cause toward the media leg");
    to_mrf.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// alice hangs up on her early dialog stating a cause while the clip plays
/// (RFC 3261 §15): the BYE that releases the confirmed media leg relays none of
/// it, while every other end-to-end header she stated still rides.
#[tokio::test(start_paused = true)]
async fn a_relayed_cause_never_reaches_a_media_leg() {
    let (h, alice, mrf, b2bua) = announcement_scene("b2bua-relayed-cause-media-leg", 5183).await;
    let (mut call, _mrf_dialog) = hear_the_clip(&alice, &mrf, &b2bua).await;

    let mut bye = call
        .send_request(InDialogMethod::Bye)
        .with_header("Reason", "Q.850;cause=16")
        .with_header("User-to-User", "3132;encoding=hex")
        .send()
        .await;
    bye.expect(200).await;
    call.expect(487).await;

    let mut to_mrf = mrf.receive("BYE").await;
    assert!(reasons(to_mrf.request()).is_empty(), "no cause toward the media leg");
    assert_eq!(
        to_mrf
            .request()
            .raw_text(HeaderName::from("User-to-User"))
            .next()
            .map(|v| v.as_str().to_string())
            .as_deref(),
        Some("3132;encoding=hex"),
        "the rest of the release still rides"
    );
    to_mrf.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
