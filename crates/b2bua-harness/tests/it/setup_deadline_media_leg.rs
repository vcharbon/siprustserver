//! **An answered media-server leg ends the call's setup.** The setup deadline
//! guards the setup while no leg holds a confirmed dialog; a media server
//! answering the leg an announcement service parks is such a dialog, so a
//! deadline falling while the announcement plays reaches no rule: the caller,
//! still in early media, hears the announcement out and is then answered by
//! the callee.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::TimerType;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const DEST_SDP: &str = "v=0\r\no=dest 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CALL_ID: &str = "setup-deadline-media-leg@127.0.0.1";
const FROM_TAG: &str = "alice-tag";
const SETUP_MS: u64 = 10_000;
const MRF_PORT: u16 = 5670;
const DEST_PORT: u16 = 5070;

/// A route that defers to the announcement service: a media leg to the media
/// server first, then the callee.
fn announcement_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", DEST_PORT);
                r.no_answer_ms = None;
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "clip-1",
                        "mrf_host": "127.0.0.1",
                        "mrf_port": MRF_PORT,
                        "dest_host": "127.0.0.1",
                        "dest_port": DEST_PORT,
                        "defer_routing": true,
                    }),
                );
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

#[tokio::test(start_paused = true)]
async fn an_answered_media_server_leg_ends_the_setup_deadline() {
    let h = Harness::new("setup-deadline-media-leg");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let dest = h.agent("dest", &format!("127.0.0.1:{DEST_PORT}")).await;
    let b2bua = B2buaSut::builder(announcement_decision())
        .services(vec![announcement::service()])
        .tune(|c| {
            c.setup_timeout_ms = SETUP_MS as i64;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&dest)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;

    // The announcement plays past the deadline: its fire reaches no rule.
    h.advance(Duration::from_millis(SETUP_MS + 2_000)).await;
    let playing = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(playing.a_leg.invite_final_sent(), None, "the caller still hears the announcement");
    assert!(
        playing.timers.iter().all(|t| t.timer_type() != &TimerType::SetupTimeout),
        "the fired deadline left the ledger"
    );

    // The clip played: the media leg is released and the callee dialled, who
    // rings and answers.
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
    let mut done = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await;
    done.expect(200).await;
    mrf.receive("BYE").await.respond(200, "OK").await;
    let mut dest_uas = dest.receive("INVITE").await;
    dest_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    dest_uas.respond(200, "OK").with_sdp(DEST_SDP).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
