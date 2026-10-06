//! The asserted identity under a concealing `Privacy` (RFC 3323 §5, RFC 3325
//! §5 / §7) across every relay the back-to-back UA performs: the originated
//! INVITE, a relayed provisional and final, a relayed in-dialog request and the
//! teardown. As the privacy service (the default) the B2BUA leaves the asserted
//! identity behind; inside the trust domain (`privacy_service = false`) it
//! relays every identity line beside the privacy request, for the boundary to
//! act on.

use b2bua_harness::{B2buaScene, B2buaSut};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::{SipRequest, SipResponse};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

const CALLER_PAI: &str = "<sip:+15550001@op.example>";
const CALLER_PPI: &str = "<sip:+15550001@ua.example>";
const CALLER_RPI: &str = "<sip:+15550001@op.example>;party=calling;privacy=full";
const CALLEE_PAI: &str = "<sip:+15550002@op.example>";
const CALLEE_RPI: &str = "<sip:+15550002@op.example>;party=called;privacy=full";

const IDENTITY: [&str; 3] = ["P-Asserted-Identity", "P-Preferred-Identity", "Remote-Party-ID"];

fn req_lines(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

fn resp_lines(resp: &SipResponse, name: &str) -> Vec<String> {
    resp.raw(HeaderName::from(name)).map(str::to_string).collect()
}

async fn scene(name: &str, privacy_service: bool) -> B2buaScene {
    scene_relaying(name, privacy_service, &[]).await
}

/// A scene whose deployment also names `relay_headers` to copy onto the
/// originated INVITE.
async fn scene_relaying(name: &str, privacy_service: bool, relay_headers: &[&str]) -> B2buaScene {
    let relay_headers: Vec<String> = relay_headers.iter().map(|h| h.to_string()).collect();
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(move |c| {
            c.privacy_service = privacy_service;
            c.relay_headers = relay_headers;
        })
    })
    .await
}

/// What one full call shows on the far side of each relay, `Privacy: id` on
/// every message: the INVITE, the 180, the 200, bob's offerless re-INVITE and
/// alice's 200 to it, and alice's BYE. Each entry is (message, header, lines seen).
async fn identity_seen_across_a_call(
    s: &B2buaScene,
) -> Vec<(&'static str, &'static str, Vec<String>)> {
    let mut seen = Vec::new();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLER_PAI)
        .with_header("P-Preferred-Identity", CALLER_PPI)
        .with_header("Remote-Party-ID", CALLER_RPI)
        .through(s.b2bua.addr)
        .send()
        .await;

    let mut uas = s.bob.receive("INVITE").await;
    for name in IDENTITY {
        seen.push(("INVITE", name, req_lines(uas.request(), name)));
    }
    assert_eq!(req_lines(uas.request(), "Privacy"), ["id"], "the privacy request travels");

    uas.respond(180, "Ringing")
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLEE_PAI)
        .with_header("Remote-Party-ID", CALLEE_RPI)
        .await;
    let ringing = call.expect(180).await;
    seen.push(("180", "P-Asserted-Identity", resp_lines(&ringing, "P-Asserted-Identity")));
    seen.push(("180", "Remote-Party-ID", resp_lines(&ringing, "Remote-Party-ID")));

    uas.respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLEE_PAI)
        .with_header("Remote-Party-ID", CALLEE_RPI)
        .await;
    let ok = call.expect(200).await;
    seen.push(("200", "P-Asserted-Identity", resp_lines(&ok, "P-Asserted-Identity")));
    seen.push(("200", "Remote-Party-ID", resp_lines(&ok, "Remote-Party-ID")));
    assert_eq!(resp_lines(&ok, "Privacy"), ["id"], "the privacy request travels");
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinvite = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLEE_PAI)
        .with_header("Remote-Party-ID", CALLEE_RPI)
        .send()
        .await;
    let mut alice_uas = s.alice.receive("INVITE").await;
    seen.push((
        "re-INVITE",
        "P-Asserted-Identity",
        req_lines(alice_uas.request(), "P-Asserted-Identity"),
    ));
    seen.push(("re-INVITE", "Remote-Party-ID", req_lines(alice_uas.request(), "Remote-Party-ID")));
    alice_uas
        .respond(200, "OK")
        .with_sdp(REOFFER)
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLER_PAI)
        .with_header("Remote-Party-ID", CALLER_RPI)
        .await;
    let reinvite_ok = reinvite.expect(200).await;
    seen.push((
        "re-INVITE 200",
        "P-Asserted-Identity",
        resp_lines(&reinvite_ok, "P-Asserted-Identity"),
    ));
    seen.push(("re-INVITE 200", "Remote-Party-ID", resp_lines(&reinvite_ok, "Remote-Party-ID")));
    bob_dialog.ack(Some(REANSWER)).await;
    s.alice.receive("ACK").await;

    let mut bye = alice_dialog
        .send_request(InDialogMethod::Bye)
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLER_PAI)
        .with_header("Remote-Party-ID", CALLER_RPI)
        .send()
        .await;
    let mut bob_bye = s.bob.receive("BYE").await;
    seen.push(("BYE", "P-Asserted-Identity", req_lines(bob_bye.request(), "P-Asserted-Identity")));
    seen.push(("BYE", "Remote-Party-ID", req_lines(bob_bye.request(), "Remote-Party-ID")));
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;
    seen
}

/// The privacy service (the default) leaves every asserted identity behind on
/// every relay, while the privacy request itself travels.
#[tokio::test(start_paused = true)]
async fn the_privacy_service_withholds_the_asserted_identity_on_every_relay() {
    let s = scene("identity-privacy-service-on", true).await;
    for (message, name, lines) in identity_seen_across_a_call(&s).await {
        assert_eq!(lines, Vec::<String>::new(), "{name} withheld on the relayed {message}");
    }
    let _report = s.finish().await;
}

/// Inside the trust domain every identity line rides verbatim beside the
/// privacy request, on every relay.
#[tokio::test(start_paused = true)]
async fn inside_the_trust_domain_the_asserted_identity_rides_on_every_relay() {
    let s = scene("identity-privacy-service-off", false).await;
    for (message, name, lines) in identity_seen_across_a_call(&s).await {
        let sent = match (message, name) {
            ("INVITE" | "re-INVITE 200" | "BYE", "P-Asserted-Identity") => CALLER_PAI,
            ("INVITE", "P-Preferred-Identity") => CALLER_PPI,
            ("INVITE" | "re-INVITE 200" | "BYE", "Remote-Party-ID") => CALLER_RPI,
            (_, "P-Asserted-Identity") => CALLEE_PAI,
            (_, _) => CALLEE_RPI,
        };
        assert_eq!(lines, [sent], "{name} relayed on the {message}");
    }
    let _report = s.finish().await;
}

/// The identity lines bob's INVITE carries when the deployment names both
/// identity headers in `relay_headers`, the caller's INVITE asking
/// `Privacy: id`. The call completes and tears down.
async fn identity_under_a_configured_relay(s: &B2buaScene) -> [Vec<String>; 2] {
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLER_PAI)
        .with_header("Remote-Party-ID", CALLER_RPI)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    let seen = [
        req_lines(uas.request(), "P-Asserted-Identity"),
        req_lines(uas.request(), "Remote-Party-ID"),
    ];
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    seen
}

const CONFIGURED: [&str; 2] = ["P-Asserted-Identity", "Remote-Party-ID"];

/// A name the deployment configures to ride reaches no further than the relay
/// itself: as the privacy service, the identity a `Privacy: id` conceals stays
/// behind even when `relay_headers` names it.
#[tokio::test(start_paused = true)]
async fn a_configured_relay_name_does_not_reach_past_the_privacy_service() {
    let s = scene_relaying("identity-privacy-configured-on", true, &CONFIGURED).await;
    let [pai, rpi] = identity_under_a_configured_relay(&s).await;
    assert_eq!(pai, Vec::<String>::new(), "P-Asserted-Identity withheld");
    assert_eq!(rpi, Vec::<String>::new(), "Remote-Party-ID withheld");
    let _report = s.finish().await;
}

/// Inside the trust domain a configured identity name rides once, verbatim.
#[tokio::test(start_paused = true)]
async fn inside_the_trust_domain_a_configured_identity_name_rides_once() {
    let s = scene_relaying("identity-privacy-configured-off", false, &CONFIGURED).await;
    let [pai, rpi] = identity_under_a_configured_relay(&s).await;
    assert_eq!(pai, [CALLER_PAI]);
    assert_eq!(rpi, [CALLER_RPI]);
    let _report = s.finish().await;
}

/// bob refuses with 486 under `Privacy: id`, carrying both identity headers;
/// what alice's 486 carries of them.
async fn identity_on_a_relayed_failing_final(s: &B2buaScene) -> [Vec<String>; 2] {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", CALLEE_PAI)
        .with_header("Remote-Party-ID", CALLEE_RPI)
        .await;
    s.bob.receive("ACK").await;
    let busy = call.expect(486).await;
    assert_eq!(resp_lines(&busy, "Privacy"), ["id"], "the privacy request travels");
    [resp_lines(&busy, "P-Asserted-Identity"), resp_lines(&busy, "Remote-Party-ID")]
}

/// A failing final relayed straight through (no failure consult) leaves the
/// asserted identity behind as the privacy service.
#[tokio::test(start_paused = true)]
async fn the_privacy_service_withholds_the_identity_on_a_relayed_failing_final() {
    let s = scene("identity-privacy-486-on", true).await;
    let [pai, rpi] = identity_on_a_relayed_failing_final(&s).await;
    assert_eq!(pai, Vec::<String>::new());
    assert_eq!(rpi, Vec::<String>::new());
    let _report = s.finish().await;
}

/// Inside the trust domain the failing final's identity rides verbatim.
#[tokio::test(start_paused = true)]
async fn inside_the_trust_domain_a_relayed_failing_finals_identity_rides() {
    let s = scene("identity-privacy-486-off", false).await;
    let [pai, rpi] = identity_on_a_relayed_failing_final(&s).await;
    assert_eq!(pai, [CALLEE_PAI]);
    assert_eq!(rpi, [CALLEE_RPI]);
    let _report = s.finish().await;
}
