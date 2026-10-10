//! Every in-dialog request is answered by what its dialog is (RFC 3261 §12.2.2,
//! §15): a To-tag and From-tag naming no dialog the B2BUA holds on the leg the
//! request arrived on draw 481, whatever the leg; a dialog whose BYE
//! completed takes nothing more, not even a BYE. A dialog this stack is
//! ending answers in itself (`ending_dialog`).
//!
//! A request whose dialog exists but whose relay has nowhere to go yet is
//! answered too, never left to its sender's Timer F: a peer leg with no dialog
//! to carry it, a peer dialog its party was never shown, or no peer leg while
//! the call lives draw 500 with `Retry-After` (§14.2, RFC 3311 §5.2: retry
//! later, the dialog stays); a request from a leg a service owns that no rule
//! takes draws 403. An UPDATE with no relayable peer keeps its 491.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallFailureResponse, NewCallResponse, RouteDecision, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, stated_by_response, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::SipResponse;

use crate::caller_facing_early_dialogs::own_then_stated;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// A tag no party was ever given.
const STRANGER: &str = "no-dialog-here";
/// The To-tag of the media server's early dialog, and of its answer.
const MRF_TAG: &str = "mrf-early";

/// The B2BUA and two parties at automatic addresses, the B2BUA routing every
/// call to bob.
async fn scene(name: &str, build: impl FnOnce(&Agent) -> b2bua_harness::B2buaSutBuilder) -> Scene {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = build(&bob).start(&h, "b2bua", &h.next_addr().to_string()).await;
    Scene { h, alice, bob, b2bua }
}

struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
}

impl Scene {
    async fn finish(self) {
        settle_until(|| self.b2bua.is_reaped()).await;
        self.alice.drain().await;
        self.bob.drain().await;
        self.b2bua.assert_fully_reaped();
        let _ = self.h.finish().await;
    }
}

fn route_to_bob(bob: &Agent) -> b2bua_harness::B2buaSutBuilder {
    B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
}

/// The refusal is the retry-later one: 500 with `Retry-After` (RFC 3261 §14.2,
/// §20.33).
fn assert_retry_later(resp: &SipResponse, what: &str) {
    assert_eq!(resp.status(), 500, "{what}");
    assert!(
        stated_by_response(resp, "Retry-After").is_some_and(|v| v.trim() == "1"),
        "{what}: the 500 states Retry-After: 1"
    );
}

/// A caller request under a To-tag the B2BUA never gave her names no dialog:
/// 481, nothing reaches the callee, and the call goes on.
#[tokio::test(start_paused = true)]
async fn a_caller_request_under_an_unknown_to_tag_is_refused_481() {
    let s = scene("dialog-gate-caller-unknown-to-tag", route_to_bob).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    s.h.allow_violation(
        "mid-dialog-tags",
        "the INFO under a foreign To-tag is the deviation under test (RFC 3261 §12.2.2)",
    );
    let before = dialog.local_cseq();
    let mut info = dialog.send_request(InDialogMethod::Info).with_to_tag(STRANGER).send().await;
    dialog.set_local_cseq(before);
    info.expect(481).await;
    s.h.advance(Duration::from_millis(100)).await;
    assert!(s.bob.try_receive_tolerating("INFO", &[]).await.is_none(), "nothing crossed");

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    s.finish().await;
}

/// A ringing callee's request under a From-tag of no early dialog it created
/// names no dialog (RFC 3261 §12.2.2): 481, and the caller is offered nothing.
#[tokio::test(start_paused = true)]
async fn an_early_callee_request_under_an_unknown_from_tag_is_refused_481() {
    let s = scene("dialog-gate-callee-unknown-from-tag", route_to_bob).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    call.expect(180).await;

    s.h.allow_violation(
        "mid-dialog-tags",
        "the INFO under a From-tag of no dialog is the deviation under test (RFC 3261 §12.2.2)",
    );
    uas.adopt_to_tag(STRANGER);
    let mut ghost = uas.dialog();
    ghost.send_request(InDialogMethod::Info).send().await.expect(481).await;
    s.h.advance(Duration::from_millis(100)).await;
    assert!(s.alice.try_receive_tolerating("INFO", &[]).await.is_none(), "nothing crossed");

    uas.adopt_to_tag("bobfork1");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    s.finish().await;
}

/// Between two attempts of a reroute the caller's early dialog fronts a
/// callee dialog the rejection ended and the next attempt has none yet: her
/// INFO is answered 500 with `Retry-After` (RFC 3261 §14.2), her UPDATE 491
/// (RFC 3311 §5.2); the call goes on and the second attempt answers.
#[tokio::test(start_paused = true)]
async fn a_caller_request_between_reroute_attempts_is_told_to_retry() {
    let h = Harness::with_transit_delay("dialog-gate-between-reroute-attempts", 1);
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent_auto("bob2").await;
    let (first, second) = (bob1.addr(), bob2.addr());
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&first.ip().to_string(), first.port());
                r.callback_context = Some("reroute".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                if req.failure.failed_leg_id.as_deref() == Some("b-1") {
                    let mut r = route_to(&second.ip().to_string(), second.port());
                    r.callback_context = Some("reroute".into());
                    CallFailureResponse::Route(r)
                } else {
                    CallFailureResponse::Relay { label: None }
                }
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", &h.next_addr().to_string()).await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    uas1.respond(486, "Busy Here").await;
    bob1.receive("ACK").await;
    let mut uas2 = bob2.receive("INVITE").await;

    let info = call.send_request(InDialogMethod::Info).send().await.expect(500).await;
    assert_retry_later(&info, "an INFO with no callee dialog to carry it");
    call.send_request(InDialogMethod::Update).send().await.expect(491).await;
    h.advance(Duration::from_millis(100)).await;
    assert!(bob2.try_receive_tolerating("INFO", &[]).await.is_none(), "nothing crossed");

    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A fork the B2BUA gave up — its no-answer deadline passed and it was
/// cancelled while the call reroutes — keeps its early dialog until the
/// INVITE's final (RFC 3261 §9.1, §15.1.2): an INFO its callee sends there
/// is answered 200 and an UPDATE 487, neither reaching anyone; once the 487
/// ended the dialog an INFO draws 481. The reroute answers the caller.
#[tokio::test(start_paused = true)]
async fn a_fork_being_cancelled_answers_its_early_dialog_until_the_invites_final() {
    let h = Harness::with_transit_delay("dialog-gate-fork-given-up", 1);
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent_auto("bob2").await;
    let (first, second) = (bob1.addr(), bob2.addr());
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&first.ip().to_string(), first.port());
                r.no_answer_timeout_sec = Some(30);
                r.callback_context = Some("reroute".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |_req| {
                CallFailureResponse::Route(route_to(&second.ip().to_string(), second.port()))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_secs(30) + Duration::from_millis(300)).await;
    bob1.receive("CANCEL").await.respond(200, "OK").await;

    let mut early = uas1.dialog();
    early.send_request(InDialogMethod::Info).send().await.expect(200).await;
    early.send_request(InDialogMethod::Update).send().await.expect(487).await;
    uas1.respond(487, "Request Terminated").await;
    bob1.receive("ACK").await;
    allow_a_request_past_the_rejection(&h);
    early.send_request(InDialogMethod::Info).send().await.expect(481).await;
    h.advance(Duration::from_millis(100)).await;
    assert!(alice.try_receive_tolerating("INFO", &[]).await.is_none(), "no INFO crossed");
    assert!(alice.try_receive_tolerating("UPDATE", &[]).await.is_none(), "no UPDATE crossed");

    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A request on an early dialog its INVITE's non-2xx final ended is the
/// deviation `mid-dialog-tags` judges (RFC 3261 §12.1).
fn allow_a_request_past_the_rejection(h: &Harness) {
    h.allow_violation(
        "mid-dialog-tags",
        "the INFO on an early dialog the 487 ended is the deviation under test (RFC 3261 §12.1)",
    );
}

/// The caller's early dialog is a service's own: the callee's provisional
/// rang no dialog of hers. Her INFO in it has no callee dialog to ride: 500
/// with `Retry-After`, and the callee's answer completes the call.
#[tokio::test(start_paused = true)]
async fn a_caller_request_with_no_callee_dialog_yet_is_told_to_retry() {
    let s = scene("dialog-gate-no-callee-dialog-yet", |bob| {
        route_to_bob(bob).services(vec![own_then_stated::service_def()])
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let info = call.send_request(InDialogMethod::Info).send().await.expect(500).await;
    assert_retry_later(&info, "an INFO with no callee dialog to carry it");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    s.finish().await;
}

/// Under the first-18x mask the caller is shown one early dialog; a second
/// callee fork rings behind it. That fork's INFO has no caller dialog it was
/// shown under: 500 with `Retry-After`, nothing reaches the caller, and the
/// fork's answer opens her dialog of its own.
#[tokio::test(start_paused = true)]
async fn a_callee_fork_the_caller_was_never_shown_is_told_to_retry() {
    let s = scene("dialog-gate-unshown-fork", |bob| {
        B2buaSut::route_all_to_with_18x(
            &bob.addr().ip().to_string(),
            bob.addr().port(),
            RelayFirst18xStrategy::DropSdp,
        )
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;

    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();
    let info = fork2.send_request(InDialogMethod::Info).send().await.expect(500).await;
    assert_retry_later(&info, "an INFO from a fork the caller was never shown");
    s.h.advance(Duration::from_millis(100)).await;
    assert!(s.alice.try_receive_tolerating("INFO", &[]).await.is_none(), "nothing crossed");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    s.finish().await;
}

/// A route that plays an announcement first: a media leg to `mrf`, then the
/// destination `dest`.
fn announcement_route(mrf: SocketAddr, dest: SocketAddr) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r: RouteDecision = route_to(&dest.ip().to_string(), dest.port());
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "clip-1",
                        "mrf_host": mrf.ip().to_string(),
                        "mrf_port": mrf.port(),
                        "dest_host": dest.ip().to_string(),
                        "dest_port": dest.port(),
                        "defer_routing": true,
                    }),
                );
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// A media leg belongs to its service, which relays nothing of it: an OPTIONS
/// in its early dialog that no rule takes is refused 403 (RFC 3261 §21.4.4).
/// The service then plays and BYEs the media leg; once that BYE completed the
/// dialog is over, so the media server's late BYE draws 481 (§15.1.2) and the
/// call to the destination goes on.
#[tokio::test(start_paused = true)]
async fn a_media_leg_request_no_rule_takes_is_refused_and_its_late_bye_481() {
    let h = Harness::with_transit_delay("dialog-gate-media-leg", 1);
    let alice = h.agent_auto("alice").await;
    let mrf = h.agent_auto("mrf").await;
    let dest = h.agent_auto("dest").await;
    let b2bua = B2buaSut::builder(announcement_route(mrf.addr(), dest.addr()))
        .services(vec![::announcement::service()])
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&dest).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(180, "Ringing").with_to_tag(MRF_TAG).await;
    mrf_uas.adopt_to_tag(MRF_TAG);
    let mut mrf_early = mrf_uas.dialog();
    mrf_early.send_request(InDialogMethod::Options).send().await.expect(403).await;

    mrf_uas.respond(200, "OK").with_to_tag(MRF_TAG).with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_early;
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let done_body = String::from_utf8(::announcement::mscml::build_response(200)).unwrap();
    mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await
        .expect(200)
        .await;
    mrf.receive("BYE").await.respond(200, "OK").await;

    let mut dest_uas = dest.receive("INVITE").await;
    h.allow_violation(
        "mid-dialog-tags",
        "the BYE on a dialog whose BYE completed is the deviation under test (RFC 3261 §15.1.2)",
    );
    mrf_dialog.bye().await.expect(481).await;

    dest_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    mrf.drain().await;
    dest.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A response whose From-tag is no tag this stack put on the leg answers no
/// request of this stack's: it is dropped before any rule reads it, and the
/// caller is sent nothing.
#[tokio::test(start_paused = true)]
async fn a_response_to_no_request_of_this_stack_is_dropped() {
    let s = scene("dialog-gate-stray-response", route_to_bob).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let invite = uas.request().clone();
    s.h.allow_violation(
        "mid-dialog-tags",
        "the response under a From-tag of no request is the deviation under test",
    );
    let wire = format!(
        "SIP/2.0 183 Session Progress\r\n\
         Via: {via}\r\n\
         From: <{from}>;tag={STRANGER}\r\n\
         To: <{to}>;tag=stray-fork\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Content-Length: 0\r\n\r\n",
        via = invite.raw_text(sip_message::header::HeaderName::Via).next().unwrap().as_str(),
        from = invite.from().uri(),
        to = invite.to().uri(),
        call_id = invite.call_id().as_str(),
        cseq = invite.cseq().seq(),
    );
    s.bob.try_send_datagram(wire.as_bytes(), s.b2bua.addr).await.expect("the 183 leaves");
    s.h.advance(Duration::from_millis(100)).await;
    assert_eq!(s.alice.drain_tu().await, 0, "a response to nothing is relayed to no one");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    s.finish().await;
}

/// A transferee leg this stack dialled for a REFER is its transfer's own
/// until realigned; a request on it under a From-tag of no dialog it created
/// names no dialog (RFC 3261 §12.2.2): 481, whichever service owns the leg.
#[tokio::test(start_paused = true)]
async fn a_transferee_request_under_an_unknown_from_tag_is_refused_481() {
    let h = Harness::with_transit_delay("dialog-gate-transferee-unknown-from-tag", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let charlie = h.agent_auto("charlie").await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"{}","port":{}}}}}"#,
        charlie.addr().ip(),
        charlie.addr().port()
    );
    let refer_to = format!("<sip:charlie@{}>", charlie.addr());
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to)
        .with_header("X-Api-Call", &api)
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    let mut realign = charlie.receive("INVITE").await;

    h.allow_violation(
        "mid-dialog-tags",
        "the INFO under a From-tag of no dialog is the deviation under test (RFC 3261 §12.2.2)",
    );
    h.allow_violation(
        "in-dialog-from-tag",
        "the INFO under a From-tag of no dialog is the deviation under test (RFC 3261 §12.2.2)",
    );
    charlie_uas.adopt_to_tag(STRANGER);
    let mut ghost = charlie_uas.dialog();
    ghost.send_request(InDialogMethod::Info).send().await.expect(481).await;

    realign.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    alice.receive("INVITE").await.respond(200, "OK").with_sdp(OFFER).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    charlie.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
