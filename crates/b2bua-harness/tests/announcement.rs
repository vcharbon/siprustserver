//! ADR-0016 slice 8 capstone — the out-of-tree `announcement` early-media
//! service, exercised end to end through a real `B2buaCore`.
//!
//! `announcement` depends on `b2bua-sdk` alone (no `b2bua`), and is injected
//! into the SUT via `B2buaSut::builder(..).services(..)`. The flow:
//! alice ↔ b2bua ↔ {MRF media server, destination} — early media from the MRF,
//! an MSCML control channel, then a BYE+dial+bridge to the real destination.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const DEST_SDP: &str = "v=0\r\no=dest 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const MRF_PORT: u16 = 5670;
const DEST_PORT: u16 = 5950;

/// A decision that requests an announcement: it defers normal routing and hands
/// the service its config (clip + MRF + the real destination) via `service_ext`.
fn announcement_decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", DEST_PORT);
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "intro-001",
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

#[tokio::test]
async fn announcement_happy_path() {
    let h = Harness::with_transit_delay("announcement-happy", 1);
    let alice = h.agent("alice", "127.0.0.1:5901").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let dest = h.agent("dest", &format!("127.0.0.1:{DEST_PORT}")).await;
    let b2bua = B2buaSut::builder(announcement_decision())
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5921")
        .await;

    // Alice calls; routing is deferred and the service launches a media leg to
    // the MRF.
    let mut call = alice.invite(&dest).with_sdp(OFFER).through(b2bua.addr).send().await;

    // The MRF answers the media leg (the B2BUA offered it alice's SDP).
    let mut mrf_uas = mrf.receive("INVITE").await;
    assert_eq!(String::from_utf8_lossy(&mrf_uas.request().body), OFFER, "MRF gets alice's offer");
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();

    // Alice receives a 183 early-media carrying the MRF's SDP (RFC 5009 PEM).
    let pem = call.expect(183).await;
    assert_eq!(String::from_utf8_lossy(&pem.body), MRF_SDP, "183 brokers the MRF SDP to A");
    assert_eq!(get_header(&pem.headers, "p-early-media").unwrap_or(""), "sendrecv");

    // The B2BUA opens the MSCML control channel toward the MRF: INFO <play>.
    let mut play = mrf.receive("INFO").await;
    assert_eq!(
        get_header(&play.request().headers, "content-type").unwrap_or(""),
        "application/mediaservercontrol+xml",
    );
    assert!(
        String::from_utf8_lossy(&play.request().body).contains("href=\"intro-001\""),
        "INFO carries the MSCML <play> for the clip",
    );
    play.respond(200, "OK").await;

    // The MRF reports the clip finished (MSCML <response code="200">).
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
    let mut done = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await;
    done.expect(200).await;

    // The B2BUA BYEs the media leg and dials the real destination.
    mrf.receive("BYE").await.respond(200, "OK").await;
    let mut dest_uas = dest.receive("INVITE").await;
    assert_eq!(String::from_utf8_lossy(&dest_uas.request().body), OFFER, "destination gets alice's offer");
    dest_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    dest_uas.respond(200, "OK").with_sdp(DEST_SDP).await;

    // Alice is answered with the destination's SDP and bridged.
    let final_200 = call.expect(200).await;
    assert_eq!(String::from_utf8_lossy(&final_200.body), DEST_SDP, "A answered with the destination SDP");
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;

    // Teardown: alice hangs up → the destination is BYE'd.
    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

// ── Failure path: the MRF rejects the media leg → the call terminates cleanly.
#[tokio::test]
async fn announcement_mrf_rejects() {
    let h = Harness::with_transit_delay("announcement-mrf-rejects", 1);
    let alice = h.agent("alice", "127.0.0.1:5902").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let b2bua = B2buaSut::builder(announcement_decision())
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5922")
        .await;

    let mut call = alice.invite(&mrf).with_sdp(OFFER).through(b2bua.addr).send().await;

    // The MRF declines the announcement leg.
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(503, "Service Unavailable").await;
    mrf.receive("ACK").await; // the b2bua completes the MRF's reject txn (§17.1.1.3)

    // The caller's INVITE is failed (the MRF's status relayed) and the call ends.
    let failed = call.expect(503).await;
    assert_eq!(failed.status, 503);

    let _ = h.finish().await;
    assert_eq!(b2bua.active_calls(), 0, "the call is reaped");
}

// ── Reject-teardown AFTER the media leg answered (newkahneed-027 regression).
// The MRF answers (alice gets 183 early media, a parked *unadopted* media leg),
// then the clip fails (MSCML <response> non-2xx). The service rejects the caller
// with a 4xx on its early dialog and terminates. The caller never got a 2xx, so
// the a-leg must be resolved by that 4xx — NOT BYE'd. Before the generic fix,
// `confirm-dialog` on the media 200 spuriously marked the a-leg `Confirmed`, so
// `BeginTermination` tried to BYE a dialog alice never established → undeliverable
// BYE, the call stranded in `Terminating`, and its CDR never flushed.
#[tokio::test]
async fn announcement_clip_fails_after_answer_rejects_caller_without_bye() {
    let h = Harness::with_transit_delay("announcement-clip-fails", 1);
    let alice = h.agent("alice", "127.0.0.1:5904").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let b2bua = B2buaSut::builder(announcement_decision())
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5924")
        .await;

    let mut call = alice.invite(&mrf).with_sdp(OFFER).through(b2bua.addr).send().await;

    // MRF answers the media leg; alice gets early media; the MSCML <play> flies.
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;

    // The MRF reports the clip FAILED (MSCML <response code="480"> — a
    // max-duration/no-answer abort). The service rejects the caller.
    let fail_body = String::from_utf8(announcement::mscml::build_response(480)).unwrap();
    let mut failed_info = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&fail_body)
        .send()
        .await;
    failed_info.expect(200).await; // the INFO is answered

    // Alice gets her 4xx final on the early dialog (mapped from the MSCML code) —
    // a real INVITE final, not a BYE on a phantom confirmed dialog.
    let rejected = call.expect(480).await;
    assert_eq!(rejected.status, 480, "caller rejected with the announced 4xx");

    // Only the (confirmed) media leg is BYE'd by the teardown.
    mrf.receive("BYE").await.respond(200, "OK").await;

    let _ = h.finish().await;
    assert_eq!(b2bua.active_calls(), 0, "the call reaps — no stranded a-leg BYE");
}

// ── Post-reject crossing BYE (newkahneed-028 regression). Same reject-teardown
// as above, but the MRF's own BYE crosses the b2bua's teardown BYE on the wire.
// Before the fix, the reject turn left the a-leg unresolved (`RelayFailureToALeg`/
// `RespondToALeg` are wire-only, and `BeginTermination`'s a-leg arm only set
// `ByeDisposition::None`), so the turn consuming the crossing BYE reached
// `→ terminated` with a "still-unanswered" a-leg and the ADR-0022 invariant
// re-answered a spurious 503 (INVITE) — a second final on an already-rejected
// INVITE. Now `BeginTermination` resolves the just-answered a-leg to
// `Terminated`, and the invariant stays a pure safety net.
//
// Two assertion lanes, deliberately:
//  - the CDR must NOT carry the `unanswered_at_termination` 503 synthesis —
//    this is the discriminating check (pre-fix it fails);
//  - the wire must show exactly one final toward the caller. In this compact
//    flow the a-leg INVITE server txn is still `Completed` when the spurious
//    503 fires, so sip-txn's idempotence backstop absorbs the wire copy — but
//    in a long early-media flow (RBT max-duration; the txn swept at ~193 s)
//    `do_send_response` falls through to a RAW send and the 503 reaches the
//    caller, which is how newkahsip observed it. State must be right, not
//    backstop-dependent.
#[tokio::test]
async fn crossing_bye_after_reject_gets_200_and_no_second_final_to_caller() {
    let h = Harness::with_transit_delay("announcement-reject-crossing-bye", 1);
    let alice = h.agent("alice", "127.0.0.1:5905").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let b2bua = B2buaSut::builder(announcement_decision())
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5925")
        .await;

    let mut call = alice.invite(&mrf).with_sdp(OFFER).through(b2bua.addr).send().await;

    // MRF answers the media leg; alice gets early media; the MSCML <play> flies.
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;

    // The MRF reports the clip FAILED → the service rejects the caller with a
    // 480 on its early dialog and begins termination (BYE toward the media leg).
    let fail_body = String::from_utf8(announcement::mscml::build_response(480)).unwrap();
    let mut failed_info = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&fail_body)
        .send()
        .await;
    failed_info.expect(200).await;
    let rejected = call.expect(480).await;
    assert_eq!(rejected.status, 480, "caller rejected with the announced 4xx");

    // The MRF hangs up on its own — its BYE crosses the b2bua's teardown BYE
    // (already in flight toward the MRF) on the wire.
    let mut mrf_bye = mrf_dialog.bye().await;
    // The b2bua's teardown BYE is confirmed as usual…
    mrf.receive("BYE").await.respond(200, "OK").await;
    // …and the b2bua answers the crossing BYE 200 (`resolve-cross-bye`).
    mrf_bye.expect(200).await;

    // Drain the async teardown (reap + buffered CDR write), then finish (RFC
    // audit) + the leak oracle.
    settle_until(|| b2bua.active_calls() == 0 && b2bua.cdr_records().len() == 1).await;
    let alice_addr = alice.addr();
    let report = h.finish().await;

    // THE regression (discriminating check): the a-leg was resolved by its
    // just-sent 4xx, so the ADR-0022 `unanswered_at_termination` 503 synthesis
    // must not fire — not into the CDR, and not toward the wire.
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    let spurious: Vec<_> = cdrs[0]
        .events
        .iter()
        .filter(|e| e.reason.as_deref() == Some("unanswered_at_termination"))
        .collect();
    assert!(
        spurious.is_empty(),
        "the rejected a-leg is resolved — no ADR-0022 503 synthesis: {spurious:?}",
    );

    // And the wire: exactly one final toward alice — her 480, no second final.
    let finals_to_alice: Vec<u16> = report
        .entries()
        .iter()
        .filter(|e| e.to == alice_addr)
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r)) if r.status >= 200 => Some(r.status),
            _ => None,
        })
        .collect();
    assert_eq!(
        finals_to_alice,
        vec![480],
        "exactly one final (the 480) toward the caller — no spurious second final",
    );
    b2bua.assert_fully_reaped();
}

// ── A-side hangup mid-announcement: alice CANCELs before the bridge → ordinary
// →Terminated cleanup BYEs the unadopted media leg (no special rule).
#[tokio::test]
async fn announcement_caller_cancels_mid_clip() {
    let h = Harness::with_transit_delay("announcement-caller-cancels", 1);
    let alice = h.agent("alice", "127.0.0.1:5903").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let b2bua = B2buaSut::builder(announcement_decision())
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5923")
        .await;

    let mut call = alice.invite(&mrf).with_sdp(OFFER).through(b2bua.addr).send().await;

    // MRF answers; alice gets early media; the MSCML <play> is in flight.
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;

    // Alice hangs up before the clip finishes → CANCEL (200) then 487 INVITE.
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // The confirmed (unadopted) media leg is BYE'd by the ordinary teardown —
    // no announcement rule is involved (the generic termination reaps it).
    mrf.receive("BYE").await.respond(200, "OK").await;

    let _ = h.finish().await;
}
