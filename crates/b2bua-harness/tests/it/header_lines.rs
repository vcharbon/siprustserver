//! A decision states a header as LINES (RFC 3261 §7.3.1): a multi-instance
//! header such as `Diversion` (RFC 5806) or `History-Info` (RFC 7044) keeps
//! one wire line per entry and the stated order, on the b-leg INVITE the route
//! mints and on the final a reject authors. A name the decision states owns
//! every relayed line of it; `null` keeps the name off the message.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, BOB_PORT};
use sip_message::header::HeaderName;
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const HOP_NEW: &str = "<sip:+15550002@redirector.example>;reason=unconditional;counter=1";
const HOP_OLD: &str = "<sip:+15550001@pbx.example>;reason=no-answer;counter=1";

async fn plan_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await
}

fn lines(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// The caller sent one `Diversion` hop and a `Privacy`; the route states two
/// `Diversion` lines (the new hop first) and removes `Privacy`. bob's INVITE
/// carries exactly the two stated lines in that order, no `Privacy`, and the
/// caller's unstated headers relayed.
#[tokio::test(start_paused = true)]
async fn a_route_states_a_multi_instance_header_as_ordered_lines() {
    let s = plan_scene("hdr-lines-route").await;
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": BOB_PORT},
        "update_headers": {
            "Diversion": [HOP_NEW, HOP_OLD],
            "Privacy": null,
            "P-Access-Network-Info": "GSTN;cc=33"
        }
    })
    .to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .with_header("Diversion", HOP_OLD)
        .with_header("Privacy", "id")
        .with_header("P-Vendor-Thing", "annotation")
        .through(s.b2bua.addr)
        .send()
        .await;

    let mut uas = s.bob.receive("INVITE").await;
    let req = uas.request();
    assert_eq!(
        lines(req, "Diversion"),
        [HOP_NEW, HOP_OLD],
        "one line per stated entry, in the stated order, the relayed hop not duplicated"
    );
    assert_eq!(lines(req, "Privacy"), Vec::<String>::new(), "a null statement keeps the name off");
    assert_eq!(lines(req, "P-Access-Network-Info"), ["GSTN;cc=33"], "a bare string is one line");
    assert_eq!(lines(req, "P-Vendor-Thing"), ["annotation"], "an unstated name relays");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// A capability half the decision removes stays off the originated INVITE:
/// the face's advertisement (the originator's relayed `Allow` / `Accept`) is
/// a statement the removal outranks, like every other relayed line.
#[tokio::test(start_paused = true)]
async fn a_removed_capability_half_is_not_re_advertised() {
    let s = plan_scene("hdr-lines-cap-removed").await;
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": BOB_PORT},
        "update_headers": {"Allow": null, "Accept": ["application/sdp, text/plain"]}
    })
    .to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .with_header("Allow", "INVITE, ACK, BYE")
        .with_header("Accept", "application/sdp")
        .through(s.b2bua.addr)
        .send()
        .await;

    let mut uas = s.bob.receive("INVITE").await;
    let req = uas.request();
    assert_eq!(lines(req, "Allow"), Vec::<String>::new(), "the removed half is not re-advertised");
    assert_eq!(lines(req, "Accept"), ["application/sdp, text/plain"], "the stated half stands");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// A reject the decision authors carries every stated line of a name: two
/// `Reason` lines (RFC 3326 allows several) in the stated order.
#[tokio::test(start_paused = true)]
async fn a_reject_states_a_multi_instance_header_as_ordered_lines() {
    let s = plan_scene("hdr-lines-reject").await;
    let plan = serde_json::json!({
        "action": "reject", "code": 603, "reason": "Decline",
        "update_headers": {"Reason": ["Q.850;cause=21", "SIP;cause=603;text=\"policy\""]}
    })
    .to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;

    let resp = call.expect(603).await;
    let reasons: Vec<String> = resp.raw(HeaderName::from("Reason")).map(str::to_string).collect();
    assert_eq!(reasons, ["Q.850;cause=21", "SIP;cause=603;text=\"policy\""]);

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}
