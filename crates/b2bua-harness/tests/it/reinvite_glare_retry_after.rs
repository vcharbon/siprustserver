//! The answer to a re-INVITE that meets glare, and its `Retry-After` (RFC 3261
//! §14.2, §20.33).
//!
//! - The sender's earlier INVITE has no final yet: 500 with a `Retry-After` of
//!   0 to 10 s (§14.2, first clause), whatever range is configured.
//! - Its earlier INVITE has a 2xx not yet ACKed: 491, with a value from
//!   `glare_retry_after` when one is configured, none otherwise.
//! - An INVITE of the stack's own is in progress on that face (a crossing, or
//!   the stack's ACK not yet sent): 491 with no `Retry-After` (§14.2, second
//!   clause).

use std::sync::Arc;
use std::time::Duration;

use b2bua::config::RetryAfterRange;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallReleaseResponse, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, stated_by_response, B2buaSut, B2buaSutBuilder};
use call::ReleaseEventKind;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";
const REOFFER2: &str = "v=0\r\no=bob 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n";

const FIXED: RetryAfterRange = RetryAfterRange { min_sec: 2, max_sec: 2 };
const SPREAD: RetryAfterRange = RetryAfterRange { min_sec: 3, max_sec: 7 };
/// A configured range outside RFC 3261 §14.2's 0 to 10 s: a 500 that took it
/// would show.
const BEYOND_RFC: RetryAfterRange = RetryAfterRange { min_sec: 60, max_sec: 60 };

/// A B2BUA routing every call to `127.0.0.1:callee_port`, under `range`.
fn sut(callee_port: u16, range: Option<RetryAfterRange>) -> B2buaSutBuilder {
    B2buaSut::route_all_to("127.0.0.1", callee_port).tune(move |c| c.glare_retry_after = range)
}

/// The waiver for a party that re-INVITEs while its own earlier INVITE is open:
/// the peer behaviour these tests interwork with.
fn waive_overtaking(h: &Harness, party: &str) {
    h.waive(
        WaiverScope::rule(
            "no-re-invite-while-invite-in-progress",
            "the party deliberately re-INVITEs while its earlier INVITE is still open — \
             the peer behaviour this test exists to interwork with",
        )
        .on_party(party),
    );
}

/// The caller re-INVITEs before ACKing the call's own 2xx, so the INVITE she
/// sent is still open here: the 491 carries the configured value.
async fn caller_overtakes_the_initial_ack(
    name: &str,
    ports: (&str, u16, &str),
    range: Option<RetryAfterRange>,
) -> Option<String> {
    let (alice_addr, bob_port, b2bua_addr) = ports;
    let h = Harness::with_transit_delay(name, 0);
    waive_overtaking(&h, "alice");
    let alice = h.agent("alice", alice_addr).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let b2bua = sut(bob_port, range).start(&h, "b2bua", b2bua_addr).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    let mut early = call.send_request(InDialogMethod::Invite).with_sdp(REOFFER2).send().await;
    let refused = early.expect(491).await;

    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    stated_by_response(&refused, "Retry-After")
}

#[tokio::test]
async fn a_newcomer_before_the_initial_ack_carries_the_configured_retry_after() {
    let ra = caller_overtakes_the_initial_ack(
        "b2bua-glare-ra-initial-ack",
        ("127.0.0.1:6300", 6301, "127.0.0.1:6302"),
        Some(FIXED),
    )
    .await;
    assert_eq!(ra.as_deref(), Some("2"));
}

#[tokio::test]
async fn without_a_range_the_491_carries_no_retry_after() {
    let ra = caller_overtakes_the_initial_ack(
        "b2bua-glare-ra-off",
        ("127.0.0.1:6303", 6304, "127.0.0.1:6305"),
        None,
    )
    .await;
    assert_eq!(ra, None);
}

/// Bob re-INVITEs again before ACKing the 2xx to his first re-INVITE: the 491
/// carries a value inside the configured range.
#[tokio::test]
async fn a_newcomer_before_its_reinvite_ack_carries_a_value_in_the_range() {
    let h = Harness::with_transit_delay("b2bua-glare-ra-reinvite-ack", 0);
    waive_overtaking(&h, "bob");
    let alice = h.agent("alice", "127.0.0.1:6306").await;
    let bob = h.agent("bob", "127.0.0.1:6307").await;
    let b2bua = sut(6307, Some(SPREAD)).start(&h, "b2bua", "127.0.0.1:6308").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv1 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let cseq1 = reinv1.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    alice.receive("INVITE").await.respond(200, "OK").with_sdp(REANSWER).await;
    reinv1.expect(200).await;

    let mut reinv2 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER2)).await;
    let refused = reinv2.expect(491).await;
    let secs: u32 = stated_by_response(&refused, "Retry-After")
        .expect("the 491 carries a Retry-After")
        .parse()
        .expect("delta-seconds");
    assert!((SPREAD.min_sec..=SPREAD.max_sec).contains(&secs), "{secs}");

    bob_dialog.ack_for(cseq1, None).await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// Bob re-INVITEs again while his first re-INVITE still awaits alice's final:
/// the stack is the UAS of an INVITE it has sent no final to, so §14.2 owes
/// 500 with a `Retry-After` of 0 to 10 s, not the configured range.
#[tokio::test]
async fn a_newcomer_while_its_first_awaits_the_final_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-glare-ra-pending-final", 0);
    waive_overtaking(&h, "bob");
    let alice = h.agent("alice", "127.0.0.1:6309").await;
    let bob = h.agent("bob", "127.0.0.1:6310").await;
    let b2bua = sut(6310, Some(BEYOND_RFC)).start(&h, "b2bua", "127.0.0.1:6311").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv1 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let cseq1 = reinv1.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;

    let mut reinv2 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER2)).await;
    let refused = reinv2.expect(500).await;
    let secs: u32 = stated_by_response(&refused, "Retry-After")
        .expect("the 500 carries a Retry-After")
        .parse()
        .expect("delta-seconds");
    assert!(secs <= 10, "{secs}");

    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv1.expect(200).await;
    bob_dialog.ack_for(cseq1, None).await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// Bob CANCELs his relayed re-INVITE (487) and re-INVITEs again before alice
/// answers the relayed copy: his own INVITE is over, only the stack's toward
/// alice is open, so the 491 carries no Retry-After.
#[tokio::test]
async fn a_newcomer_after_its_cancelled_reinvite_carries_no_retry_after() {
    let h = Harness::with_transit_delay("b2bua-glare-ra-cancelled", 0);
    let alice = h.agent("alice", "127.0.0.1:6318").await;
    let bob = h.agent("bob", "127.0.0.1:6319").await;
    let b2bua = sut(6319, Some(FIXED)).start(&h, "b2bua", "127.0.0.1:6320").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv1 = bob_dialog.reinvite(Some(REOFFER)).await;
    let mut alice_uas = alice.receive("INVITE").await;
    let mut cxl = reinv1.cancel().await;
    cxl.expect(200).await;
    reinv1.expect(487).await;

    let mut reinv2 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER2)).await;
    let refused = reinv2.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None);

    alice_uas.respond(100, "Trying").await;
    alice.receive("CANCEL").await.respond(200, "OK").await;
    alice_uas.respond(487, "Request Terminated").await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// Crossing re-INVITEs: alice's is relayed to bob, bob's crosses it. The stack's
/// own INVITE is in progress on bob's face, so its 491 carries no Retry-After.
#[tokio::test]
async fn a_crossing_reinvite_491_carries_no_retry_after() {
    let h = Harness::with_transit_delay("b2bua-glare-ra-crossing", 0);
    let alice = h.agent("alice", "127.0.0.1:6312").await;
    let bob = h.agent("bob", "127.0.0.1:6313").await;
    let b2bua = sut(6313, Some(FIXED)).start(&h, "b2bua", "127.0.0.1:6314").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut alice_reinv = alice_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut bob_uas = bob.receive("INVITE").await;
    let mut bob_reinv = bob_dialog.request(InDialogMethod::Invite, Some(REANSWER)).await;
    let refused = bob_reinv.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None);

    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    alice_reinv.expect(200).await;
    alice_dialog.ack(None).await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// Bob re-INVITEs while the 2xx he answered the call with still awaits the
/// stack's ACK. That breaks §14.1 (no new INVITE while one is in progress in
/// either direction): the deliberate deviation under test. The audit's §14.1
/// rule keys on the sender's own direction and does not flag it, so nothing is
/// waived. The stack's own INVITE is the open one, so the 491 carries no
/// Retry-After.
#[tokio::test]
async fn a_newcomer_toward_a_face_owed_our_ack_carries_no_retry_after() {
    let h = Harness::with_transit_delay("b2bua-glare-ra-owed-ack", 0);
    let alice = h.agent("alice", "127.0.0.1:6315").await;
    let bob = h.agent("bob", "127.0.0.1:6316").await;
    let b2bua = sut(6316, Some(FIXED)).start(&h, "b2bua", "127.0.0.1:6317").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut bob_dialog = uas.dialog();

    let mut early = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let refused = early.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None);

    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30005 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A release reroute is in flight while alice's first re-INVITE still awaits
/// bob's final; her second re-INVITE is glare against her own unanswered
/// INVITE, so the reroute's refusal is the §14.2 500, not a plain 491.
#[tokio::test(start_paused = true)]
async fn a_newcomer_during_a_reroute_over_its_unanswered_reinvite_is_refused_500() {
    let h = Harness::new("b2bua-glare-reroute-pending-final");
    waive_overtaking(&h, "alice");
    let alice = h.agent("alice", "127.0.0.1:6327").await;
    let bob = h.agent("bob", "127.0.0.1:6328").await;
    let media = h.agent("media", "127.0.0.1:6329").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 6328);
                r.features.platform.max_duration_sec = 60;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                ReleaseOutcome::Respond(CallReleaseResponse::Route(route_to("127.0.0.1", 6329)))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            c.glare_retry_after = Some(BEYOND_RFC);
        })
        .start(&h, "b2bua", "127.0.0.1:6330")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // Alice re-INVITEs just before the cap; bob holds his answer.
    h.advance(Duration::from_secs(59)).await;
    let mut reinv1 = alice_dialog.request(InDialogMethod::Invite, Some(REANSWER)).await;
    let cseq1 = alice_dialog.local_cseq();
    let mut bob_uas = bob.receive("INVITE").await;

    // The cap raises the release consult; the reroute dials the new target.
    h.advance(Duration::from_secs(2)).await;
    let mut media_uas = media.receive("INVITE").await;

    let mut reinv2 = alice_dialog.request(InDialogMethod::Invite, Some(REANSWER)).await;
    let refused = reinv2.expect(500).await;
    let secs: u32 = stated_by_response(&refused, "Retry-After")
        .expect("the 500 carries a Retry-After")
        .parse()
        .expect("delta-seconds");
    assert!(secs <= 10, "{secs}");

    // Bob answers alice's first re-INVITE; the reroute then completes.
    bob_uas.respond(200, "OK").with_sdp(REOFFER).await;
    reinv1.expect(200).await;
    alice_dialog.ack_for(cseq1, None).await;
    bob.receive("ACK").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    let mut bye = alice_dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
