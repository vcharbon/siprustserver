//! One session per dialog across a change of the description's author
//! (RFC 3264 §8).
//!
//! The B2BUA is one party on each dialog, whoever authored the description it
//! forwards. While a dialog's peer keeps talking to the same far party, every
//! description relays byte for byte. Once another party is spliced in (a
//! transfer target, a rerouted destination), what reaches the dialog continues
//! the session its peer already holds: the `o=` identity the peer was shown,
//! the version one above the last one sent, every m-line the dialog had kept,
//! a stream the new author does not describe kept rejected (port 0).
//!
//! `continued` builds the description the far party must receive from the one
//! the new author wrote; each assertion compares whole bodies.

use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    BodyUpdate, CallReleaseResponse, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaSut};
use call::ReleaseEventKind;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const CHARLIE_PORT: u16 = 5668;

const ALICE_OFFER: &str = "v=0\r\no=alice 101 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const BOB_ANSWER: &str = "v=0\r\no=bob 202 1 IN IP4 127.0.0.2\r\ns=-\r\nc=IN IP4 127.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 202 2 IN IP4 127.0.0.2\r\ns=-\r\nc=IN IP4 127.0.0.2\r\nt=0 0\r\nm=audio 20002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 101 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// Charlie's answer to the held offer that opens his dialog: the stream stays
/// rejected, as offered.
const CHARLIE_HELD_ANSWER: &str = "v=0\r\no=charlie 303 1 IN IP4 127.0.0.3\r\ns=-\r\nc=IN IP4 127.0.0.3\r\nt=0 0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=inactive\r\n";
const CHARLIE_ACTIVE: &str = "v=0\r\no=charlie 303 2 IN IP4 127.0.0.3\r\ns=charlie\r\nc=IN IP4 127.0.0.3\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REALIGNED: &str = "v=0\r\no=alice 101 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_REOFFER: &str = "v=0\r\no=charlie 303 3 IN IP4 127.0.0.3\r\ns=charlie\r\nc=IN IP4 127.0.0.3\r\nt=0 0\r\nm=audio 30002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_ANSWERS_CHARLIE: &str = "v=0\r\no=alice 101 4 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10006 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// `authored` as it reaches a dialog whose session is named by `origin`: the
/// `o=` line replaced, every other line as written.
fn continued(authored: &str, origin: &str) -> String {
    let o_line = authored.split("\r\n").find(|l| l.starts_with("o=")).expect("an o= line");
    authored.replacen(o_line, &format!("o={origin}"), 1)
}

/// A received description's `o=` fields: username, sess-id, version, and
/// `<nettype> <addrtype> <address>`.
fn origin_of(body: &[u8]) -> (String, String, u64, String) {
    let o = sip_message::parse_origin(body).expect("a readable o= line");
    (
        o.username,
        o.session_id,
        o.session_version,
        format!("{} {} {}", o.nettype, o.addrtype, o.unicast_address),
    )
}

fn body_of(txn: &ServerTxn) -> String {
    String::from_utf8_lossy(txn.request().body()).into_owned()
}

/// The referrer's in-dialog `REFER` authorizing a transfer to charlie.
fn x_api_allow_c() -> String {
    format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    )
}

/// A↔B relays byte for byte; B transfers A to C; afterwards A's dialog carries
/// B's session and C's dialog carries the session this stack opened toward C,
/// each one version up per description, whoever authored it — an unchanged
/// re-send included.
#[tokio::test]
async fn a_spliced_peer_continues_each_dialog_session() {
    let h = Harness::with_transit_delay("sdp-session-continuity-transfer", 1);
    let alice = h.agent("alice", "127.0.0.1:5932").await;
    let bob = h.agent("bob", "127.0.0.1:5942").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5942)
        .start(&h, "b2bua", "127.0.0.1:5952")
        .await;

    // ── A↔B: every description relays as written. ──
    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(body_of(&bob_uas), ALICE_OFFER);
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(String::from_utf8_lossy(ok.body()), BOB_ANSWER);
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // A re-offer from the same far party, then its unchanged re-send: both as
    // written, the re-send keeping the version it states.
    for _ in 0..2 {
        let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
        let mut at_alice = alice.receive("INVITE").await;
        assert_eq!(body_of(&at_alice), BOB_REOFFER, "the same peer's re-offer relays as written");
        at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
        let ok = reinvite.expect(200).await;
        assert_eq!(String::from_utf8_lossy(ok.body()), ALICE_REANSWER);
        bob_dialog.ack(None).await;
        alice.receive("ACK").await;
    }

    // ── B REFERs A to C. ──
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    // C's dialog opens on this stack's own held offer: its session is C's.
    let mut charlie_uas = charlie.receive("INVITE").await;
    let (user, sess_id, held_version, address) = origin_of(charlie_uas.request().body());
    let c_session = |version: u64| format!("{user} {sess_id} {version} {address}");
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_HELD_ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut charlie_dialog = charlie_uas.dialog();

    // The c-realign re-INVITE carries A's description under C's session.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_eq!(
        body_of(&c_realign),
        continued(ALICE_OFFER, &c_session(held_version + 1)),
        "A's description reaches C under the session C already holds, one version up",
    );
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE).await;
    charlie.receive("ACK").await;

    // The a-realign re-INVITE carries C's description under B's session.
    let mut a_realign = alice.receive("INVITE").await;
    assert_eq!(
        body_of(&a_realign),
        continued(CHARLIE_ACTIVE, "bob 202 3 IN IP4 127.0.0.2"),
        "C's description reaches A under the session A already holds, one version up",
    );
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGNED).await;
    alice.receive("ACK").await;

    // ── A↔C: both directions continue, an unchanged re-send included. ──
    for (a_version, c_version) in [(4, held_version + 2), (5, held_version + 3)] {
        let mut reinvite =
            charlie_dialog.request(InDialogMethod::Invite, Some(CHARLIE_REOFFER)).await;
        let mut at_alice = alice.receive("INVITE").await;
        assert_eq!(
            body_of(&at_alice),
            continued(CHARLIE_REOFFER, &format!("bob 202 {a_version} IN IP4 127.0.0.2")),
            "C's re-offer reaches A under B's session",
        );
        at_alice.respond(200, "OK").with_sdp(ALICE_ANSWERS_CHARLIE).await;
        let ok = reinvite.expect(200).await;
        assert_eq!(
            String::from_utf8_lossy(ok.body()),
            continued(ALICE_ANSWERS_CHARLIE, &c_session(c_version)),
            "A's answer reaches C under C's session",
        );
        charlie_dialog.ack(None).await;
        alice.receive("ACK").await;
    }

    // A answers C's next re-offer with a 183 and then the 200, both with the same
    // description: C is given it once, one version, twice (RFC 3264 §8).
    let mut reinvite = charlie_dialog.request(InDialogMethod::Invite, Some(CHARLIE_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    at_alice.respond(183, "Session Progress").with_sdp(ALICE_ANSWERS_CHARLIE).await;
    let early = reinvite.expect(183).await;
    at_alice.respond(200, "OK").with_sdp(ALICE_ANSWERS_CHARLIE).await;
    let ok = reinvite.expect(200).await;
    let same = continued(ALICE_ANSWERS_CHARLIE, &c_session(held_version + 4));
    assert_eq!(String::from_utf8_lossy(early.body()), same, "the 183 under C's session");
    assert_eq!(String::from_utf8_lossy(ok.body()), same, "the 200 repeats it at the same version");
    charlie_dialog.ack(None).await;
    alice.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}

const ALICE_AV_OFFER: &str = "v=0\r\no=alice 101 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\nm=video 10002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
const BOB_AV_ANSWER: &str = "v=0\r\no=bob 202 1 IN IP4 127.0.0.2\r\ns=-\r\nc=IN IP4 127.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
/// The replacement destination is offered audio only and answers so.
const AUDIO_ONLY_OFFER: &str = "v=0\r\no=anchor 505 1 IN IP4 127.0.0.5\r\ns=-\r\nc=IN IP4 127.0.0.5\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 404 1 IN IP4 127.0.0.4\r\ns=-\r\nc=IN IP4 127.0.0.4\r\nt=0 0\r\nm=audio 50000 RTP/AVP 0\r\n";
const MEDIA_REOFFER: &str = "v=0\r\no=media 404 2 IN IP4 127.0.0.4\r\ns=-\r\nc=IN IP4 127.0.0.4\r\nt=0 0\r\nm=audio 50002 RTP/AVP 0\r\n";
const ALICE_AV_ANSWER_2: &str = "v=0\r\no=alice 101 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
const ALICE_AV_REANSWER: &str = "v=0\r\no=alice 101 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";

/// An established audio+video call is rerouted to a destination that
/// describes audio only: the re-offer toward A keeps B's session and both
/// m-lines, the video stream rejected at its position (RFC 3264 §8.2). A's
/// answer to the destination's next re-offer reaches it with the destination's
/// one m-line (§6), under the session this stack opened toward it.
#[tokio::test(start_paused = true)]
async fn a_reoffer_describing_fewer_streams_keeps_the_dropped_one_rejected() {
    let h = Harness::new("sdp-session-continuity-fewer-streams");
    let alice = h.agent("alice", "127.0.0.1:5933").await;
    let bob = h.agent("bob", "127.0.0.1:5943").await;
    let media = h.agent("media", "127.0.0.1:5963").await;

    let decision = std::sync::Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5943);
                r.features.platform.max_duration_sec = 60;
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.callback_context = Some("ctx-release".into());
                NewCallResponse::Route(r)
            })
            .on_release(move |_req| {
                let mut r = route_to("127.0.0.1", 5963);
                r.update_body = BodyUpdate::Replace(AUDIO_ONLY_OFFER.to_string());
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5953")
        .await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_AV_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_AV_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // The cap expires; the release decision reroutes to the media destination.
    h.advance(Duration::from_secs(61)).await;
    let mut media_uas = media.receive("INVITE").await;
    assert_eq!(body_of(&media_uas), AUDIO_ONLY_OFFER);
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    let tag = media_uas.dialog().local_tag().to_string();
    while let Some(mut again) = media.try_receive_tolerating("INVITE", &[]).await {
        again.respond(200, "OK").with_sdp(MEDIA_ANSWER).with_to_tag(&tag).await;
    }
    media.receive("ACK").await;

    let mut a_realign = alice.receive("INVITE").await;
    assert_eq!(
        body_of(&a_realign),
        format!(
            "{}m=video 0 RTP/AVP 96\r\n",
            continued(MEDIA_ANSWER, "bob 202 2 IN IP4 127.0.0.2")
        ),
        "the re-offer keeps B's session and the video m-line, rejected",
    );
    a_realign.respond(200, "OK").with_sdp(ALICE_AV_REANSWER).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    let mut media_dialog = media_uas.dialog();
    let mut reinvite = media_dialog.request(InDialogMethod::Invite, Some(MEDIA_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    assert_eq!(
        body_of(&at_alice),
        format!(
            "{}m=video 0 RTP/AVP 96\r\n",
            continued(MEDIA_REOFFER, "bob 202 3 IN IP4 127.0.0.2")
        ),
    );
    at_alice.respond(200, "OK").with_sdp(ALICE_AV_ANSWER_2).await;
    let ok = reinvite.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        "v=0\r\no=anchor 505 2 IN IP4 127.0.0.5\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n",
        "A's answer reaches the destination with its one m-line, under the session opened toward it",
    );
    media_dialog.ack(None).await;
    alice.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}

/// An OPTIONS answer describes capabilities, not the session (RFC 3261 §11.2):
/// its description relays as written, whatever session it names, and the
/// dialog's session is untouched by it.
#[tokio::test]
async fn an_options_answer_stays_outside_the_session() {
    const CAPABILITIES: &str = "v=0\r\no=gateway 909 1 IN IP4 127.0.0.9\r\ns=-\r\nc=IN IP4 127.0.0.9\r\nt=0 0\r\nm=audio 0 RTP/AVP 0 8\r\n";
    let h = Harness::with_transit_delay("sdp-session-continuity-options", 1);
    let alice = h.agent("alice", "127.0.0.1:5934").await;
    let bob = h.agent("bob", "127.0.0.1:5944").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5944).start(&h, "b2bua", "127.0.0.1:5954").await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut options = bob_dialog.send_request(InDialogMethod::Options).send().await;
    alice.receive("OPTIONS").await.respond(200, "OK").with_sdp(CAPABILITIES).await;
    let ok = options.expect(200).await;
    assert_eq!(String::from_utf8_lossy(ok.body()), CAPABILITIES, "relayed as written");

    let mut reinvite = alice_dialog.request(InDialogMethod::Invite, Some(ALICE_REANSWER)).await;
    let mut at_bob = bob.receive("INVITE").await;
    assert_eq!(body_of(&at_bob), ALICE_REANSWER, "the caller's session continues as written");
    at_bob.respond(200, "OK").with_sdp(BOB_REOFFER).await;
    reinvite.expect(200).await;
    alice_dialog.ack(None).await;
    bob.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}

/// A failure final's description states capabilities, not the session (RFC
/// 3261 §13.2.1, §21.4.26): it reaches the offerer as written, and the next
/// description of the dialog's author still relays as written.
#[tokio::test]
async fn a_failure_answer_body_stays_outside_the_session() {
    const CAPABILITIES: &str = "v=0\r\no=gateway 909 1 IN IP4 127.0.0.9\r\ns=-\r\nc=IN IP4 127.0.0.9\r\nt=0 0\r\nm=audio 0 RTP/AVP 0 8\r\n";
    let h = Harness::with_transit_delay("sdp-session-continuity-488", 1);
    let alice = h.agent("alice", "127.0.0.1:5935").await;
    let bob = h.agent("bob", "127.0.0.1:5945").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5945).start(&h, "b2bua", "127.0.0.1:5955").await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refused = alice_dialog.request(InDialogMethod::Invite, Some(ALICE_REANSWER)).await;
    bob.receive("INVITE").await.respond(488, "Not Acceptable Here").with_sdp(CAPABILITIES).await;
    let nak = refused.expect(488).await;
    assert_eq!(String::from_utf8_lossy(nak.body()), CAPABILITIES, "relayed as written");
    bob.receive("ACK").await;

    let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    assert_eq!(body_of(&at_alice), BOB_REOFFER, "the callee's session continues as written");
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}

/// The route decision replaces the caller's offer with one of its own: the
/// callee's dialog carries that session, the stack's, so the caller's later
/// re-offer reaches the callee restated under it — even where the two share a
/// sess-id.
#[tokio::test]
async fn a_replaced_initial_offer_opens_a_session_of_the_stacks_own() {
    const REPLACEMENT: &str = "v=0\r\no=anchor 101 7 IN IP4 127.0.0.5\r\ns=-\r\nc=IN IP4 127.0.0.5\r\nt=0 0\r\nm=audio 40000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
    let h = Harness::with_transit_delay("sdp-session-continuity-replaced-offer", 1);
    let alice = h.agent("alice", "127.0.0.1:5936").await;
    let bob = h.agent("bob", "127.0.0.1:5946").await;
    let decision = std::sync::Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5946);
                r.update_body = BodyUpdate::Replace(REPLACEMENT.to_string());
                NewCallResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5956").await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(body_of(&bob_uas), REPLACEMENT);
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = alice_dialog.request(InDialogMethod::Invite, Some(ALICE_REANSWER)).await;
    let mut at_bob = bob.receive("INVITE").await;
    assert_eq!(
        body_of(&at_bob),
        continued(ALICE_REANSWER, "anchor 101 8 IN IP4 127.0.0.5"),
        "the caller's re-offer continues the session the decision's offer opened",
    );
    at_bob.respond(200, "OK").with_sdp(BOB_REOFFER).await;
    reinvite.expect(200).await;
    alice_dialog.ack(None).await;
    bob.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}

/// What crosses an early dialog stays that dialog's state through its
/// confirmation: the caller's answer to the callee's early UPDATE, relayed
/// back into the dialog it came from, is what the callee holds once that
/// dialog confirms. A later description of hers under another session id is
/// restated one version above THAT answer (RFC 3264 §8), not above her INVITE.
#[tokio::test]
async fn an_answer_relayed_into_an_early_dialog_is_what_it_confirms_with() {
    let h = Harness::with_transit_delay("sdp-session-early-relayed-answer", 0);
    let alice = h.agent("alice", "127.0.0.1:5691").await;
    let bob = h.agent("bob", "127.0.0.1:5692").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5692).start(&h, "b2bua", "127.0.0.1:5693").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(ALICE_OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(BOB_ANSWER)
        .await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut bob_dialog = uas.dialog();
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("UPDATE").await;
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    let answered = update.expect(200).await;
    assert_eq!(String::from_utf8_lossy(answered.body()), ALICE_REANSWER, "relayed as written");

    // The INVITE's offer was answered in the reliable 183 (RFC 3262 §5).
    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Her own description under another session id: restated above what bob holds.
    let moved = "v=0\r\no=alice 909 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
    let mut reinv = dialog.reinvite(Some(moved)).await;
    let cseq = dialog.local_cseq();
    let mut at_bob = bob.receive("INVITE").await;
    assert_eq!(
        body_of(&at_bob),
        continued(moved, "alice 101 3 IN IP4 127.0.0.1"),
        "one above the answer bob holds in the dialog that confirmed",
    );
    at_bob.respond(200, "OK").with_sdp(&BOB_REOFFER.replace("202 2", "202 3")).await;
    reinv.expect(200).await;
    dialog.ack_for(cseq, None).await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// After B transfers A to C, C's offerless re-INVITEs reach A offerless, and
/// A's 2xx carries the offer (RFC 3264 §5). Each such INVITE transaction is a
/// new exchange on C's dialog: A's unchanged description reaches C one version
/// above the previous one each time (RFC 3264 §8), and C's answer in the ACK
/// reaches A under B's session.
#[tokio::test]
async fn each_offerless_reinvite_after_a_splice_is_a_new_version() {
    let h = Harness::with_transit_delay("sdp-session-continuity-offerless-reinvite", 1);
    let alice = h.agent("alice", "127.0.0.1:5937").await;
    let bob = h.agent("bob", "127.0.0.1:5947").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5947)
        .start(&h, "b2bua", "127.0.0.1:5957")
        .await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    let (user, sess_id, held_version, address) = origin_of(charlie_uas.request().body());
    let c_session = |version: u64| format!("{user} {sess_id} {version} {address}");
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_HELD_ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut charlie_dialog = charlie_uas.dialog();

    let mut c_realign = charlie.receive("INVITE").await;
    assert_eq!(body_of(&c_realign), continued(ALICE_OFFER, &c_session(held_version + 1)));
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    assert_eq!(body_of(&a_realign), continued(CHARLIE_ACTIVE, "bob 202 2 IN IP4 127.0.0.2"));
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGNED).await;
    alice.receive("ACK").await;

    // ── C's offerless re-INVITEs: A offers the same description each time. ──
    for (c_version, a_version) in [(held_version + 2, 3), (held_version + 3, 4)] {
        let mut reinvite = charlie_dialog.request(InDialogMethod::Invite, None).await;
        let mut at_alice = alice.receive("INVITE").await;
        assert!(at_alice.request().body().is_empty(), "the re-INVITE reaches A offerless");
        at_alice.respond(200, "OK").with_sdp(ALICE_REALIGNED).await;
        let ok = reinvite.expect(200).await;
        assert_eq!(
            String::from_utf8_lossy(ok.body()),
            continued(ALICE_REALIGNED, &c_session(c_version)),
            "A's unchanged offer reaches C one version above the previous 2xx",
        );
        charlie_dialog.ack(Some(CHARLIE_ACTIVE)).await;
        let ack = alice.receive("ACK").await;
        assert_eq!(
            body_of(&ack),
            continued(CHARLIE_ACTIVE, &format!("bob 202 {a_version} IN IP4 127.0.0.2")),
            "C's answer reaches A under B's session",
        );
    }

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let _ = h.finish().await;
}
