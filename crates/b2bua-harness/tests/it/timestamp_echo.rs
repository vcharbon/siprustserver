//! A request's `Timestamp` comes back on the 100 Trying that answers it (RFC
//! 3261 §8.2.6.1 MUST). A `Timestamp` is a reading of its sender's clock
//! (§20.38): a relayed request whose source stated one states this stack's
//! own reading, and a relayed response whose source stated an echo echoes the
//! request this stack received on that side, with no delay. A response the
//! stack mints echoes nothing past the 100, and a response whose peer stated
//! no echo carries none.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{stated_by_response, B2buaScene, B2buaSut};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

fn timestamp(resp: &SipResponse) -> Option<String> {
    stated_by_response(resp, "Timestamp")
}

/// The initial INVITE's 100 echoes its Timestamp; the relayed 180 and 200,
/// whose callee stated no echo, do not, nor does the 200 this stack mints for
/// the caller's BYE. The BYE minted from hers states this stack's reading.
#[tokio::test(start_paused = true)]
async fn the_initial_invite_100_echoes_the_timestamp_and_nothing_else_does() {
    let s = B2buaScene::new("b2bua-timestamp-initial").await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Timestamp", "54")
        .through(s.b2bua.addr)
        .send()
        .await;
    let trying = call.expect(100).await;
    assert_eq!(timestamp(&trying).as_deref(), Some("54"), "§8.2.6.1: the 100 echoes it");

    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    assert_eq!(timestamp(&ringing), None, "the relayed 180 echoes nothing");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(timestamp(&ok), None, "the relayed 200 echoes nothing");

    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bye =
        dialog.send_request(InDialogMethod::Bye).with_header("Timestamp", "61.5").send().await;
    let mut relayed_bye = s.bob.receive("BYE").await;
    let ours = relayed_bye.request().raw(HeaderName::Timestamp).next().map(str::to_string);
    assert!(ours.as_deref().is_some_and(is_clock_reading), "the BYE minted from hers: {ours:?}");
    assert_ne!(ours.as_deref(), Some("61.5"), "this stack's reading, not hers");
    relayed_bye.respond(200, "OK").await;
    let bye_ok = bye.expect(200).await;
    assert_eq!(timestamp(&bye_ok), None, "the stack's own 200 to the BYE echoes nothing");

    let _report = s.finish().await;
}

/// RFC 3261 §20.38 `1*DIGIT [ "." *DIGIT ]`: a clock reading with no delay.
fn is_clock_reading(value: &str) -> bool {
    let (secs, frac) = value.split_once('.').unwrap_or((value, ""));
    !secs.is_empty()
        && secs.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

/// The INVITE the callee receives states this stack's own clock where the
/// caller's stated hers; the callee's echo of it comes back to the caller as
/// the echo of her own value, once per response.
#[tokio::test(start_paused = true)]
async fn the_relay_restates_the_request_and_echoes_the_caller() {
    let s = B2buaScene::new("b2bua-timestamp-peer-echo").await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Timestamp", "54")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    let stamped: Vec<String> =
        uas.request().raw(HeaderName::Timestamp).map(str::to_string).collect();
    assert_eq!(stamped.len(), 1, "one reading: {stamped:?}");
    assert!(is_clock_reading(&stamped[0]), "this stack's clock, no delay: {stamped:?}");
    assert_ne!(stamped[0], "54", "not the caller's reading");
    let echo_of_ours = format!("{} 0.25", stamped[0]);
    uas.respond(180, "Ringing").with_header("Timestamp", &echo_of_ours).await;
    let ringing = call.expect(180).await;
    assert_eq!(ringing.raw(HeaderName::Timestamp).collect::<Vec<_>>(), ["54"]);

    uas.respond(200, "OK").with_sdp(ANSWER).with_header("Timestamp", &echo_of_ours).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.raw(HeaderName::Timestamp).collect::<Vec<_>>(), ["54"]);

    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// In the dialog: a re-INVITE's 100 echoes its Timestamp and the relayed 200,
/// whose callee stated no echo, carries none; the relayed 200 to an INFO
/// whose callee echoed echoes the caller's INFO, not the callee's reading.
#[tokio::test(start_paused = true)]
async fn in_dialog_only_the_reinvite_100_echoes_the_timestamp() {
    let s = B2buaScene::new("b2bua-timestamp-in-dialog").await;
    let mut dialog = s.establish().await;

    let mut reinv = dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Timestamp", "70")
        .send()
        .await;
    let trying = reinv.expect(100).await;
    assert_eq!(timestamp(&trying).as_deref(), Some("70"), "§8.2.6.1: the 100 echoes it");
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(REANSWER).await;
    let ok = reinv.expect(200).await;
    assert_eq!(timestamp(&ok), None, "the relayed re-INVITE 200 echoes nothing");
    dialog.ack(None).await;
    s.bob.receive("ACK").await;

    let mut info =
        dialog.send_request(InDialogMethod::Info).with_header("Timestamp", "80").send().await;
    let mut relayed = s.bob.receive("INFO").await;
    let ours = relayed.request().raw(HeaderName::Timestamp).next().map(str::to_string);
    assert!(ours.as_deref().is_some_and(is_clock_reading), "this stack's reading: {ours:?}");
    relayed.respond(200, "OK").with_header("Timestamp", "1 0.5").await;
    let info_ok = info.expect(200).await;
    assert_eq!(timestamp(&info_ok).as_deref(), Some("80"), "the caller's INFO, echoed");

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// A final the stack authors from a reject decision echoes nothing; the 100
/// before it still does.
#[tokio::test(start_paused = true)]
async fn a_minted_reject_echoes_no_timestamp() {
    let s = B2buaScene::with_b2bua("b2bua-timestamp-reject", |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await;
    let plan =
        serde_json::json!({"action": "reject", "code": 603, "reason": "Decline"}).to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .with_header("Timestamp", "54")
        .through(s.b2bua.addr)
        .send()
        .await;
    let trying = call.expect(100).await;
    assert_eq!(timestamp(&trying).as_deref(), Some("54"), "§8.2.6.1: the 100 echoes it");
    let reject = call.expect(603).await;
    assert_eq!(timestamp(&reject), None, "the minted 603 echoes nothing");

    let _report = s.finish().await;
}

/// A response echoes only a request that stated a reading: where the caller's
/// INVITE stated none, the callee's echo has nothing to answer for her.
#[tokio::test(start_paused = true)]
async fn no_echo_where_the_request_stated_none() {
    let s = B2buaScene::new("b2bua-timestamp-none").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_eq!(uas.request().raw(HeaderName::Timestamp).count(), 0, "nothing to restate");
    uas.respond(200, "OK").with_sdp(ANSWER).with_header("Timestamp", "3 0.1").await;
    let ok = call.expect(200).await;
    assert_eq!(timestamp(&ok), None, "no echo of a reading she never stated");

    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
