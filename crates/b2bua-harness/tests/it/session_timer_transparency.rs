//! The session timer through the back-to-back UA (RFC 4028).
//!
//! The stack runs no session timer of its own: it neither refreshes a session
//! nor enforces its expiry. It is transparent to the timer instead, by
//! relaying both the headers that negotiate it and the re-INVITE / UPDATE
//! refreshes that keep it alive, so the two endpoints negotiate and refresh
//! end to end as if nothing stood between them.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, B2buaSutBuilder, BOB_PORT};
use call::features::{AdvertiseCapabilitiesFeature, AdvertisedCapabilities, RelayFirst18xStrategy};
use scenario_harness::agent::{Invite, ServerTxn};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{HeaderName, Supported};
use sip_message::{SipRequest, SipResponse};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE_DATE: &str = "Mon, 05 Oct 2026 10:00:00 GMT";
const BOB_DATE: &str = "Mon, 05 Oct 2026 10:00:01 GMT";

fn request_values(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// The request states one `Timestamp`, this stack's own clock reading in
/// place of the sender's `theirs` (RFC 3261 §20.38).
#[track_caller]
fn assert_restamped(req: &SipRequest, theirs: &str) {
    let stamps = request_values(req, "Timestamp");
    assert_eq!(stamps.len(), 1, "one reading: {stamps:?}");
    let (secs, frac) = stamps[0].split_once('.').unwrap_or((&stamps[0], ""));
    assert!(
        !secs.is_empty()
            && secs.bytes().all(|b| b.is_ascii_digit())
            && frac.bytes().all(|b| b.is_ascii_digit()),
        "a clock reading with no delay: {stamps:?}"
    );
    assert_ne!(stamps[0], theirs, "this stack's reading, not the sender's");
}

fn response_values(resp: &SipResponse, name: &str) -> Vec<String> {
    resp.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// The caller offers the timer as the refresher and demands that the UAS
/// support it; the callee accepts. The callee sees the caller's negotiation,
/// the caller sees the callee's answer, and each refresh, from either side,
/// crosses with its own interval. A request's clock stamps ride with it.
#[tokio::test(start_paused = true)]
async fn the_session_timer_is_negotiated_and_refreshed_end_to_end() {
    let s = B2buaScene::new("session-timer-end-to-end").await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_header("Min-SE", "90")
        .with_header("Timestamp", "54")
        .with_header("Date", ALICE_DATE)
        .through(s.b2bua.addr)
        .send()
        .await;

    let mut uas = s.bob.receive("INVITE").await;
    let invite = uas.request().clone();
    assert_eq!(request_values(&invite, "Session-Expires"), ["1800;refresher=uac"]);
    assert_eq!(request_values(&invite, "Min-SE"), ["90"]);
    assert_eq!(request_values(&invite, "Require"), ["timer"], "the UAS is the one asked");
    assert_restamped(&invite, "54");
    assert_eq!(request_values(&invite, "Date"), [ALICE_DATE]);

    uas.respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_header("Date", BOB_DATE)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(response_values(&ok, "Session-Expires"), ["1800;refresher=uac"]);
    assert_eq!(response_values(&ok, "Require"), ["timer"], "RFC 4028 §9: the UAS states it");
    assert_eq!(response_values(&ok, "Date"), [BOB_DATE]);
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // The caller refreshes with a re-INVITE (RFC 4028 §7.4).
    let mut refresh = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Supported", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_header("Min-SE", "90")
        .with_header("Timestamp", "77")
        .send()
        .await;
    let mut bob_refresh = s.bob.receive("INVITE").await;
    assert_eq!(request_values(bob_refresh.request(), "Session-Expires"), ["1800;refresher=uac"]);
    assert_eq!(request_values(bob_refresh.request(), "Min-SE"), ["90"]);
    assert_restamped(bob_refresh.request(), "77");
    bob_refresh
        .respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_header("Timestamp", "77")
        .await;
    let refreshed = refresh.expect(200).await;
    assert_eq!(response_values(&refreshed, "Session-Expires"), ["1800;refresher=uac"]);
    assert_eq!(
        response_values(&refreshed, "Timestamp"),
        ["77"],
        "the callee's echo of the caller's Timestamp rides once (RFC 3261 §8.2.6.1)"
    );
    assert_eq!(response_values(&refreshed, "Require"), ["timer"]);
    alice_dialog.ack(None).await;
    s.bob.receive("ACK").await;

    // The callee takes the refresher role over with an UPDATE (RFC 4028 §7.4).
    let mut update = bob_dialog
        .send_request(InDialogMethod::Update)
        .with_header("Supported", "timer")
        .with_header("Session-Expires", "1200;refresher=uac")
        .send()
        .await;
    let mut alice_update = s.alice.receive("UPDATE").await;
    assert_eq!(request_values(alice_update.request(), "Session-Expires"), ["1200;refresher=uac"]);
    alice_update
        .respond(200, "OK")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1200;refresher=uac")
        .await;
    let updated = update.expect(200).await;
    assert_eq!(response_values(&updated, "Session-Expires"), ["1200;refresher=uac"]);
    assert_eq!(response_values(&updated, "Require"), ["timer"]);

    let mut bye = alice_dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one call, one CDR");
    let _report = s.finish().await;
}

/// A callee that finds the interval too small answers 422 with its floor
/// (RFC 4028 §6), and the caller retries above it (§7.3). The floor must reach
/// the caller, or the retry has nothing to go on.
#[tokio::test(start_paused = true)]
async fn a_refused_interval_reaches_the_caller_with_the_callee_floor() {
    let s = B2buaScene::new("session-timer-422").await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Supported", "timer")
        .with_header("Session-Expires", "60")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_eq!(request_values(uas.request(), "Session-Expires"), ["60"]);
    uas.respond(422, "Session Interval Too Small").with_header("Min-SE", "90").await;
    s.bob.receive("ACK").await;
    let refused = call.expect(422).await;
    assert_eq!(response_values(&refused, "Min-SE"), ["90"], "the 422 states the floor");

    // The retry, a new INVITE at the floor, sets up and tears down cleanly.
    let mut retry = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Supported", "timer")
        .with_header("Session-Expires", "90")
        .with_header("Min-SE", "90")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_eq!(request_values(uas.request(), "Session-Expires"), ["90"]);
    assert_eq!(request_values(uas.request(), "Min-SE"), ["90"]);
    uas.respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header("Require", "timer")
        .with_header("Session-Expires", "90;refresher=uac")
        .await;
    let ok = retry.expect(200).await;
    assert_eq!(response_values(&ok, "Session-Expires"), ["90;refresher=uac"]);
    let mut dialog = retry.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    assert_eq!(s.b2bua.cdr_records().len(), 2, "the refused call and the retry");
    let _report = s.finish().await;
}

/// The caller's full session-timer offer, demand and clock stamps on `invite`.
fn with_timer_offer(invite: Invite<'_>) -> Invite<'_> {
    invite
        .with_sdp(OFFER)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_header("Min-SE", "90")
        .with_header("Timestamp", "54")
        .with_header("Date", ALICE_DATE)
}

/// Whether `req`'s `Supported` names `timer`.
fn supports_timer(req: &SipRequest) -> bool {
    req.header::<Supported>().is_some_and(|s| s.expect("readable Supported").contains("timer"))
}

/// `req` carries the caller's whole negotiation onward: interval, floor,
/// demand and offer.
fn assert_carries_the_negotiation(req: &SipRequest, what: &str) {
    assert_eq!(request_values(req, "Session-Expires"), ["1800;refresher=uac"], "{what}");
    assert_eq!(request_values(req, "Min-SE"), ["90"], "{what}");
    assert_eq!(request_values(req, "Require"), ["timer"], "{what}");
    assert!(supports_timer(req), "{what} offers the timer");
}

/// `req` takes no part in a session timer: no interval, no floor, no demand,
/// and no offer of an extension nobody on that leg would run.
fn assert_takes_no_part(req: &SipRequest, what: &str) {
    for name in ["Session-Expires", "Min-SE", "Require"] {
        assert!(request_values(req, name).is_empty(), "{what} carries no {name}: {req:?}");
    }
    assert!(!supports_timer(req), "{what} offers no timer: {req:?}");
}

/// A media leg the stack dials for its own purpose answers the stack, never the
/// caller, and the stack runs no timer: the leg takes no part in the caller's
/// session timer. The destination dialled after it, while the caller's INVITE
/// still waits, answers that INVITE and carries the whole negotiation. Both
/// legs leave while the caller's INVITE is pending, so its clock stamps are
/// current on both.
#[tokio::test(start_paused = true)]
async fn a_media_leg_takes_no_part_in_the_caller_session_timer() {
    const MRF_PORT: u16 = 5670;
    const DEST_PORT: u16 = 5950;
    let h = Harness::new("session-timer-media-leg");
    let alice = h.agent("alice", "127.0.0.1:5901").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let dest = h.agent("dest", &format!("127.0.0.1:{DEST_PORT}")).await;
    let decision = Arc::new(
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
    );
    let b2bua = B2buaSut::builder(decision)
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5921")
        .await;

    let mut call = with_timer_offer(alice.invite(&dest)).through(b2bua.addr).send().await;

    let mut mrf_uas = mrf.receive("INVITE").await;
    assert_takes_no_part(mrf_uas.request(), "the media leg");
    assert_restamped(mrf_uas.request(), "54");
    assert_eq!(request_values(mrf_uas.request(), "Date"), [ALICE_DATE]);
    mrf_uas.respond(200, "OK").with_sdp(ANSWER).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
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
    assert_carries_the_negotiation(dest_uas.request(), "the destination leg");
    assert_restamped(dest_uas.request(), "54");
    assert_eq!(request_values(dest_uas.request(), "Date"), [ALICE_DATE]);
    dest_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call, one CDR");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A transfer target is dialled after the caller's INVITE completed: its
/// answer never reaches the caller, nobody refreshes it, and the caller's clock
/// stamps state a moment long past. It carries none of them.
#[tokio::test(start_paused = true)]
async fn a_transfer_target_carries_neither_the_session_timer_nor_stale_clock_stamps() {
    const CHARLIE_PORT: u16 = 5667;
    let h = Harness::new("session-timer-transfer-target");
    let alice = h.agent("alice", "127.0.0.1:5902").await;
    let bob = h.agent("bob", "127.0.0.1:5912").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5912)
        .start(&h, "b2bua", "127.0.0.1:5922")
        .await;

    let mut call = with_timer_offer(alice.invite(&bob)).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_carries_the_negotiation(bob_uas.request(), "the dialled leg");
    bob_uas
        .respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    );
    bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &api)
        .send()
        .await
        .expect(202)
        .await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    assert_takes_no_part(charlie_uas.request(), "the transfer target leg");
    for name in ["Timestamp", "Date"] {
        assert!(
            request_values(charlie_uas.request(), name).is_empty(),
            "the caller's {name} states a moment long past: {:?}",
            charlie_uas.request()
        );
    }
    charlie_uas.respond(486, "Busy Here").await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call, one CDR");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A reroute leg is dialled while the caller's INVITE still waits, and its
/// answer is the caller's: it carries the whole negotiation and the clock
/// stamps, as the first attempt did.
#[tokio::test(start_paused = true)]
async fn a_reroute_leg_carries_the_caller_negotiation() {
    const CAROL_PORT: u16 = 5071;
    let s = B2buaScene::with_b2bua("session-timer-reroute", |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await;
    let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;
    let plan = serde_json::json!({
        "routes": [
            {"destination": {"host": "127.0.0.1", "port": BOB_PORT}},
            {"destination": {"host": "127.0.0.1", "port": CAROL_PORT}},
        ],
        "on_exhausted": {"action": "reject", "code": 480, "reason": "Temporarily Unavailable"},
    })
    .to_string();

    let mut call = with_timer_offer(s.alice.invite(&s.bob))
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    assert_carries_the_negotiation(bob_uas.request(), "the first attempt");
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    assert_carries_the_negotiation(carol_uas.request(), "the reroute leg");
    assert_restamped(carol_uas.request(), "54");
    assert_eq!(request_values(carol_uas.request(), "Date"), [ALICE_DATE]);
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// Under the promote-PEM strategy the callee's 2xx is absorbed: the caller was
/// answered by a synthetic 200 and never hears the callee's interval, so
/// nobody would refresh it. The strategy withholds the timer from the leg, as
/// the 180 strategies withhold `100rel`.
#[tokio::test(start_paused = true)]
async fn promote_pem_withholds_the_session_timer_from_the_callee() {
    let s = B2buaScene::with_b2bua("session-timer-promote-pem", |bob_port| {
        B2buaSut::route_all_to_with_18x(
            "127.0.0.1",
            bob_port,
            RelayFirst18xStrategy::PromotePemTo200,
        )
    })
    .await;
    // The caller states the interval and the demand without offering `timer`
    // in Supported: the strategy's withhold, not the missing offer, keeps them
    // off the leg.
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_header("Min-SE", "90")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_takes_no_part(uas.request(), "the leg whose 2xx the strategy absorbs");
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_header("Supported", "100rel, timer, replaces")
        .with_sdp(ANSWER)
        .await;
    let promoted = call.expect(200).await;
    let advertised = promoted
        .header::<Supported>()
        .expect("the callee's other tags ride")
        .expect("readable Supported");
    assert!(
        !advertised.contains("timer"),
        "the synthetic 200 states no interval, so it claims no timer (RFC 4028 §7.2)"
    );
    assert!(advertised.contains("replaces"), "the callee's other tags ride");
    let mut dialog = call.ack().await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// Accept and answer an in-dialog refresh with a plain 200.
async fn answer_refresh(mut uas: ServerTxn) {
    uas.respond(200, "OK").with_sdp(ANSWER).await;
}

/// The call withholds `timer` from the legs it originates: the leg's INVITE and
/// every refresh relayed toward it offer no timer, so they carry no interval,
/// floor or demand either (RFC 4028 §7.1: a UAC using the timer lists it in
/// `Supported`).
#[tokio::test(start_paused = true)]
async fn a_withheld_timer_tag_withdraws_the_session_timer_on_every_relay() {
    let s = B2buaScene::with_b2bua("session-timer-withheld-tag", |bob_port| {
        B2buaSut::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.features.withhold_option_tags = Some(vec!["timer".into()]);
                    NewCallResponse::Route(r)
                })
                .build(),
        ))
    })
    .await;
    let mut call = with_timer_offer(s.alice.invite(&s.bob)).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_takes_no_part(uas.request(), "the leg the call withholds the timer from");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    // The refresh demands the timer without offering it in Supported: the
    // call's withhold, not the missing offer, keeps the negotiation off.
    let mut refresh = dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .send()
        .await;
    let bob_refresh = s.bob.receive("INVITE").await;
    assert_takes_no_part(bob_refresh.request(), "the refresh relayed toward that leg");
    answer_refresh(bob_refresh).await;
    refresh.expect(200).await;
    dialog.ack(None).await;
    s.bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// A face whose declared `Supported` lacks `timer` claims no timer on that leg,
/// so the caller's interval, floor and demand stay behind with the tag.
#[tokio::test(start_paused = true)]
async fn a_declared_supported_without_timer_withdraws_the_session_timer() {
    let s = B2buaScene::with_b2bua("session-timer-declared-face", |bob_port| {
        B2buaSut::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.features.advertise_capabilities = Some(AdvertiseCapabilitiesFeature {
                        toward_originator: None,
                        toward_originated: Some(AdvertisedCapabilities {
                            allow: None,
                            supported: Some(vec!["replaces".into()]),
                        }),
                    });
                    NewCallResponse::Route(r)
                })
                .build(),
        ))
    })
    .await;
    let mut call = with_timer_offer(s.alice.invite(&s.bob)).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert_takes_no_part(uas.request(), "the leg whose face declares no timer");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut refresh = dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Supported", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .send()
        .await;
    let bob_refresh = s.bob.receive("INVITE").await;
    assert_takes_no_part(bob_refresh.request(), "the refresh relayed toward that face");
    answer_refresh(bob_refresh).await;
    refresh.expect(200).await;
    dialog.ack(None).await;
    s.bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// A route whose decision declares `supported` toward the callee and withholds
/// `withheld` from every leg the call originates.
fn declaring(
    bob_port: u16,
    supported: &'static [&'static str],
    withheld: &'static [&'static str],
) -> B2buaSutBuilder {
    B2buaSut::builder(Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", bob_port);
                r.features.advertise_capabilities = Some(AdvertiseCapabilitiesFeature {
                    toward_originator: None,
                    toward_originated: Some(AdvertisedCapabilities {
                        allow: None,
                        supported: Some(supported.iter().map(|t| t.to_string()).collect()),
                    }),
                });
                if !withheld.is_empty() {
                    r.features.withhold_option_tags =
                        Some(withheld.iter().map(|t| t.to_string()).collect());
                }
                NewCallResponse::Route(r)
            })
            .build(),
    ))
}

/// Establish alice ↔ bob with the caller's timer offer, then relay one refresh
/// re-INVITE from alice carrying `extra`; hand bob's view of it to `check`,
/// answer it, and hang up.
async fn relay_refresh(s: B2buaScene, extra: &[(&str, &str)], check: impl FnOnce(&SipRequest)) {
    let mut call = with_timer_offer(s.alice.invite(&s.bob)).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut refresh = dialog.send_request(InDialogMethod::Invite).with_sdp(REOFFER);
    for (name, value) in extra {
        refresh = refresh.with_header(name, value);
    }
    let mut refresh = refresh.send().await;
    let bob_refresh = s.bob.receive("INVITE").await;
    check(bob_refresh.request());
    answer_refresh(bob_refresh).await;
    refresh.expect(200).await;
    dialog.ack(None).await;
    s.bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one call, one CDR");
    let _report = s.finish().await;
}

const REFRESH: &[(&str, &str)] = &[
    ("Supported", "100rel, timer"),
    ("Require", "timer"),
    ("Session-Expires", "1800;refresher=uac"),
    ("Min-SE", "90"),
];

/// A face whose declared `Supported` names `timer` offers it on the leg, so a
/// refresh relayed toward it keeps the whole negotiation.
#[tokio::test(start_paused = true)]
async fn a_declared_supported_with_timer_keeps_the_relayed_refresh_whole() {
    let s = B2buaScene::with_b2bua("session-timer-declared-timer", |bob_port| {
        declaring(bob_port, &["timer"], &[])
    })
    .await;
    relay_refresh(s, REFRESH, |req| {
        assert_carries_the_negotiation(req, "the refresh toward a face declaring timer");
    })
    .await;
}

/// The call's withhold reaches a relayed refresh by its timer half only: the
/// leg was dialled outside the session timer, so the refresh carries none of
/// it, while `100rel`, which the peers negotiate end to end per transaction
/// (RFC 3262), rides in the declared set the generator stamps.
#[tokio::test(start_paused = true)]
async fn the_call_withhold_narrows_a_relayed_refresh_by_its_timer_half_only() {
    let s = B2buaScene::with_b2bua("session-timer-withhold-timer-half", |bob_port| {
        declaring(bob_port, &["100rel", "timer"], &["100rel", "timer"])
    })
    .await;
    relay_refresh(s, REFRESH, |req| {
        let supported: Vec<String> = request_values(req, "Supported");
        assert_eq!(supported, ["100rel"], "the declared set, less the withheld timer");
        assert!(request_values(req, "Session-Expires").is_empty(), "{req:?}");
        assert!(request_values(req, "Require").is_empty(), "{req:?}");
    })
    .await;
}

/// A call withholding `100rel` leaves a relayed refresh whole: the withhold
/// governs the INVITEs the stack originates, not a request the peers negotiate
/// end to end.
#[tokio::test(start_paused = true)]
async fn a_call_withhold_of_100rel_leaves_a_relayed_refresh_whole() {
    let s = B2buaScene::with_b2bua("session-timer-withhold-relayed", |bob_port| {
        B2buaSut::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.features.withhold_option_tags = Some(vec!["100rel".into()]);
                    NewCallResponse::Route(r)
                })
                .build(),
        ))
    })
    .await;
    relay_refresh(s, REFRESH, |req| {
        assert_eq!(request_values(req, "Supported"), ["100rel, timer"]);
        assert_carries_the_negotiation(req, "the refresh toward a leg that offers the timer");
    })
    .await;
}

/// A transfer target was dialled without the timer. Once the caller is bridged
/// to it, her refresh relayed toward it carries no timer either: the leg never
/// offered it, and a `Require: timer` would draw a 420 from a target that does
/// not support it.
#[tokio::test(start_paused = true)]
async fn a_refresh_toward_a_transfer_target_carries_no_session_timer() {
    const CHARLIE_PORT: u16 = 5667;
    const CHARLIE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
    const ALICE_REALIGN: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
    let h = Harness::new("session-timer-transfer-refresh");
    let alice = h.agent("alice", "127.0.0.1:5903").await;
    let bob = h.agent("bob", "127.0.0.1:5913").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5913)
        .start(&h, "b2bua", "127.0.0.1:5923")
        .await;

    let mut call = with_timer_offer(alice.invite(&bob)).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    );
    bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &api)
        .send()
        .await
        .expect(202)
        .await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    charlie.receive("INVITE").await.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    charlie.receive("ACK").await;
    alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;

    let mut refresh = alice_dialog.send_request(InDialogMethod::Invite).with_sdp(REOFFER);
    for (name, value) in REFRESH {
        refresh = refresh.with_header(name, value);
    }
    let mut refresh = refresh.send().await;
    let charlie_refresh = charlie.receive("INVITE").await;
    assert_takes_no_part(charlie_refresh.request(), "the refresh toward the transfer target");
    answer_refresh(charlie_refresh).await;
    refresh.expect(200).await;
    alice_dialog.ack(None).await;
    charlie.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call, one CDR");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Under fake-PRACK the call withholds `100rel` from a leg it originates
/// without an offer, never from a request relayed in dialog: a delayed-offer
/// re-INVITE keeps the caller's `100rel`.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_reinvite_keeps_the_callers_100rel_under_fake_prack() {
    let s = B2buaScene::with_b2bua("session-timer-fake-prack-offer-state", |bob_port| {
        B2buaSut::route_all_to_with_18x("127.0.0.1", bob_port, RelayFirst18xStrategy::FakePrack)
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut reinvite =
        dialog.send_request(InDialogMethod::Invite).with_header("Supported", "100rel").send().await;
    let mut bob_reinvite = s.bob.receive("INVITE").await;
    assert_eq!(request_values(bob_reinvite.request(), "Supported"), ["100rel"]);
    bob_reinvite.respond(200, "OK").with_sdp(ANSWER).await;
    reinvite.expect(200).await;
    dialog.ack(Some(REOFFER)).await;
    s.bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one call, one CDR");
    let _report = s.finish().await;
}
