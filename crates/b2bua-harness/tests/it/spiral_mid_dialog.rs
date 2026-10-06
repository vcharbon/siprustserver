//! Mid-dialog requests across a spiral (RFC 3261 §16.3): the B2BUA's
//! outgoing INVITE comes back to it through a third-party proxy as a second
//! call, so each in-dialog request crosses the B2BUA twice, under one Call-ID
//! on the leg between the two calls.
//!
//!   alice ──▶ b2bua (call 1) ──▶ proxy ──▶ b2bua (call 2) ──▶ bob
//!
//! Each scenario runs from either end, through a proxy that Record-Routes and
//! through one that does not (§16.6 step 4): without it, call 1 and call 2
//! send their in-dialog requests to each other's Contact, which is the
//! B2BUA's own address.
//!
//! - re-INVITE accepted (offer in the re-INVITE, answer in the 200), rejected
//!   488, and lost to a crossing re-INVITE, which takes 491 (§14.1);
//! - UPDATE in an early and in a confirmed dialog (RFC 3311);
//! - reliable provisionals on the spiralled INVITE (RFC 3262): the PRACK
//!   crosses both calls, each translating the RAck onto the RSeq it was shown;
//! - INFO (RFC 6086).

use b2bua_harness::B2buaScene;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{ClientInvite, Dialog};
use sip_message::generators::InDialogMethod;
use sip_message::header::{HeaderName, RAck, Require};

use scenario_harness::RunReport;

use crate::common::spiral::{
    assert_both_answered_and_ended, assert_crossed_between_the_calls, spiral_scene, write_callflow,
};
use crate::common::stateful_proxy::{RecordRoute, StatefulProxy};

const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=sendonly\r\n";
const BOB_REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=recvonly\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=sendonly\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=recvonly\r\n";

/// The RSeq bob states on his reliable provisional, far from any number the
/// B2BUA mints first, so a translated RAck is told from a relayed one.
const BOB_RSEQ: u32 = 4711;

/// Which end originates the request under test.
#[derive(Clone, Copy)]
enum End {
    Caller,
    Callee,
}

/// A spiralled call answered by bob, both ends holding their dialog.
struct Confirmed {
    s: B2buaScene,
    record_route: RecordRoute,
    _proxy: StatefulProxy,
    call_id: String,
    alice: Dialog,
    bob: Dialog,
}

impl Confirmed {
    async fn new(name: &str, record_route: RecordRoute) -> Self {
        let (s, proxy) = spiral_scene(name, record_route).await;
        let mut call =
            s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
        let mut uas = s.bob.receive("INVITE").await;
        uas.respond(180, "Ringing").await;
        call.expect(180).await;
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.expect(200).await;
        let alice = call.ack().await;
        s.bob.receive("ACK").await;
        let bob = uas.dialog();
        Self { call_id: call.call_id(), s, record_route, _proxy: proxy, alice, bob }
    }

    /// The originating end's dialog and agent, then the other end's agent.
    fn ends(&mut self, from: End) -> (&mut Dialog, &scenario_harness::Agent) {
        match from {
            End::Caller => (&mut self.alice, &self.s.bob),
            End::Callee => (&mut self.bob, &self.s.alice),
        }
    }

    /// `from` hangs up; both calls end with an answered, BYE-ended CDR, and
    /// `method` crossed between the calls on the path the proxy gives it.
    /// Returns the run's report.
    async fn hang_up_and_finish(mut self, from: End, method: &str) -> RunReport {
        let (dialog, other) = self.ends(from);
        let mut bye = dialog.bye().await;
        other.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        b2bua_harness::settle_until(|| {
            self.s.b2bua.cdr_records().len() == 2 && self.s.b2bua.is_reaped()
        })
        .await;
        assert_both_answered_and_ended(&self.s, &self.call_id);
        let report = self.s.finish().await;
        assert_crossed_between_the_calls(&report, self.record_route, method);
        report
    }
}

fn offer_and_answer(from: End) -> (&'static str, &'static str) {
    match from {
        End::Caller => (ALICE_REOFFER, BOB_REANSWER),
        End::Callee => (BOB_REOFFER, ALICE_REANSWER),
    }
}

fn has_direction(body: &[u8], attribute: &str) -> bool {
    String::from_utf8_lossy(body).contains(attribute)
}

// ── re-INVITE accepted ──────────────────────────────────────────────────────

/// A re-INVITE carrying an offer crosses both calls to the far end; its 200
/// carries the answer back, and the ACK crosses both calls again.
async fn reinvite_accepted(name: &str, record_route: RecordRoute, from: End) {
    let mut c = Confirmed::new(name, record_route).await;
    let (offer, answer) = offer_and_answer(from);
    let (dialog, other) = c.ends(from);
    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(offer)).await;
    let mut uas = other.receive("INVITE").await;
    assert!(has_direction(uas.request().body(), "a=sendonly"), "the offer crosses the spiral");
    uas.respond(200, "OK").with_sdp(answer).await;
    let ok = reinvite.expect(200).await;
    assert!(has_direction(ok.body(), "a=recvonly"), "the answer crosses the spiral back");
    dialog.ack(None).await;
    other.receive("ACK").await;
    let report = c.hang_up_and_finish(from, "INVITE").await;
    write_callflow(&report, name);
}

#[tokio::test(start_paused = true)]
async fn a_caller_reinvite_crosses_a_spiral() {
    reinvite_accepted("spiral-reinvite-caller", RecordRoute::Yes, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_crosses_a_spiral() {
    reinvite_accepted("spiral-reinvite-callee", RecordRoute::Yes, End::Callee).await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_reinvite_crosses_a_spiral_without_record_route() {
    reinvite_accepted("spiral-no-rr-reinvite-caller", RecordRoute::No, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_crosses_a_spiral_without_record_route() {
    reinvite_accepted("spiral-no-rr-reinvite-callee", RecordRoute::No, End::Callee).await;
}

// ── re-INVITE rejected 488 ──────────────────────────────────────────────────

/// The far end rejects the re-offer 488: the rejection crosses both calls,
/// each hop ACKs the one below it, and the session stays as it was (§14.1).
async fn reinvite_rejected(name: &str, record_route: RecordRoute, from: End) {
    let mut c = Confirmed::new(name, record_route).await;
    let (offer, _) = offer_and_answer(from);
    let (dialog, other) = c.ends(from);
    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(offer)).await;
    let mut uas = other.receive("INVITE").await;
    uas.respond(488, "Not Acceptable Here").await;
    uas.expect_ack().await;
    reinvite.expect(488).await;
    assert_eq!(c.s.b2bua.active_calls(), 2, "a rejected re-INVITE leaves both calls up");
    c.hang_up_and_finish(from, "INVITE").await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_reinvite_rejected_across_a_spiral() {
    reinvite_rejected("spiral-reinvite-488-caller", RecordRoute::Yes, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_rejected_across_a_spiral() {
    reinvite_rejected("spiral-reinvite-488-callee", RecordRoute::Yes, End::Callee).await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_reinvite_rejected_across_a_spiral_without_record_route() {
    reinvite_rejected("spiral-no-rr-reinvite-488-caller", RecordRoute::No, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_rejected_across_a_spiral_without_record_route() {
    reinvite_rejected("spiral-no-rr-reinvite-488-callee", RecordRoute::No, End::Callee).await;
}

// ── re-INVITE glare ─────────────────────────────────────────────────────────

/// `from` re-INVITEs first; the other end, holding that re-INVITE unanswered,
/// sends one of its own (no offer). The B2BUA call facing the other end has a
/// re-INVITE in progress on that dialog and answers 491 (§14.1); the first
/// re-INVITE then completes.
async fn reinvite_glare(name: &str, record_route: RecordRoute, from: End) {
    let mut c = Confirmed::new(name, record_route).await;
    let (offer, answer) = offer_and_answer(from);
    let (first, second) = match from {
        End::Caller => (&mut c.alice, &mut c.bob),
        End::Callee => (&mut c.bob, &mut c.alice),
    };
    let second_agent = match from {
        End::Caller => &c.s.bob,
        End::Callee => &c.s.alice,
    };
    let mut winner = first.request(InDialogMethod::Invite, Some(offer)).await;
    let mut uas = second_agent.receive("INVITE").await;
    let mut loser = second.request(InDialogMethod::Invite, None).await;
    loser.expect(491).await;
    uas.respond(200, "OK").with_sdp(answer).await;
    winner.expect(200).await;
    first.ack(None).await;
    second_agent.receive("ACK").await;
    c.hang_up_and_finish(from, "INVITE").await;
}

#[tokio::test(start_paused = true)]
async fn crossing_reinvites_across_a_spiral_the_caller_wins() {
    reinvite_glare("spiral-glare-caller", RecordRoute::Yes, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn crossing_reinvites_across_a_spiral_the_callee_wins() {
    reinvite_glare("spiral-glare-callee", RecordRoute::Yes, End::Callee).await;
}

#[tokio::test(start_paused = true)]
async fn crossing_reinvites_across_a_spiral_without_record_route_the_caller_wins() {
    reinvite_glare("spiral-no-rr-glare-caller", RecordRoute::No, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn crossing_reinvites_across_a_spiral_without_record_route_the_callee_wins() {
    reinvite_glare("spiral-no-rr-glare-callee", RecordRoute::No, End::Callee).await;
}

// ── UPDATE, confirmed dialog ────────────────────────────────────────────────

/// An UPDATE carrying an offer in the confirmed dialog crosses both calls;
/// its 200 carries the answer back (RFC 3311 §5.2).
async fn confirmed_update(name: &str, record_route: RecordRoute, from: End) {
    let mut c = Confirmed::new(name, record_route).await;
    let (offer, answer) = offer_and_answer(from);
    let (dialog, other) = c.ends(from);
    let mut update = dialog.request(InDialogMethod::Update, Some(offer)).await;
    let mut uas = other.receive("UPDATE").await;
    assert!(has_direction(uas.request().body(), "a=sendonly"), "the offer crosses the spiral");
    uas.respond(200, "OK").with_sdp(answer).await;
    let ok = update.expect(200).await;
    assert!(has_direction(ok.body(), "a=recvonly"), "the answer crosses the spiral back");
    c.hang_up_and_finish(from, "UPDATE").await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_update_crosses_a_confirmed_spiral() {
    confirmed_update("spiral-update-caller", RecordRoute::Yes, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_update_crosses_a_confirmed_spiral() {
    confirmed_update("spiral-update-callee", RecordRoute::Yes, End::Callee).await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_update_crosses_a_confirmed_spiral_without_record_route() {
    confirmed_update("spiral-no-rr-update-caller", RecordRoute::No, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_update_crosses_a_confirmed_spiral_without_record_route() {
    confirmed_update("spiral-no-rr-update-callee", RecordRoute::No, End::Callee).await;
}

// ── UPDATE, early dialog ────────────────────────────────────────────────────

/// While bob rings, `from` sends a bodyless UPDATE in the early dialog
/// (RFC 3311 §5.1); it crosses both early dialogs and its 200 comes back.
/// Bob then answers and the call ends.
async fn early_update(name: &str, record_route: RecordRoute, from: End) {
    let (s, _proxy) = spiral_scene(name, record_route).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let a_tag = ringing.to().tag().expect("the early dialog's tag on the 180").to_string();

    match from {
        End::Caller => {
            let mut update =
                call.send_request(InDialogMethod::Update).with_to_tag(&a_tag).send().await;
            s.bob.receive("UPDATE").await.respond(200, "OK").await;
            update.expect(200).await;
        }
        End::Callee => {
            let mut update = uas.dialog().request(InDialogMethod::Update, None).await;
            s.alice.receive("UPDATE").await.respond(200, "OK").await;
            update.expect(200).await;
        }
    }

    answer_and_hang_up(s, call, uas, record_route, "UPDATE").await;
}

/// bob answers the ringing spiralled call; alice ACKs and hangs up; `method`
/// crossed between the calls on the path the proxy gives it.
async fn answer_and_hang_up(
    s: B2buaScene,
    mut call: ClientInvite,
    mut uas: scenario_harness::ServerTxn,
    record_route: RecordRoute,
    method: &str,
) {
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    b2bua_harness::settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    assert_both_answered_and_ended(&s, &call.call_id());
    let report = s.finish().await;
    assert_crossed_between_the_calls(&report, record_route, method);
}

#[tokio::test(start_paused = true)]
async fn a_caller_update_crosses_an_early_spiral() {
    early_update("spiral-early-update-caller", RecordRoute::Yes, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_update_crosses_an_early_spiral() {
    early_update("spiral-early-update-callee", RecordRoute::Yes, End::Callee).await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_update_crosses_an_early_spiral_without_record_route() {
    early_update("spiral-no-rr-early-update-caller", RecordRoute::No, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_update_crosses_an_early_spiral_without_record_route() {
    early_update("spiral-no-rr-early-update-callee", RecordRoute::No, End::Callee).await;
}

// ── reliable provisionals ───────────────────────────────────────────────────

/// alice offers 100rel; bob's reliable 183 crosses both calls, each showing
/// its upstream side an RSeq of its own; alice's PRACK crosses both calls back,
/// its RAck translated onto bob's RSeq at bob (RFC 3262 §3, §4).
async fn reliable_provisional(name: &str, record_route: RecordRoute) {
    let (s, _proxy) = spiral_scene(name, record_route).await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_header("Supported", "100rel")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(ANSWER_SDP).await;
    let p183 = call.expect(183).await;
    assert!(
        p183.header::<Require>().expect("a Require").expect("readable Require").contains("100rel"),
        "the provisional reaches alice reliable",
    );
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    let mut at_bob = s.bob.receive("PRACK").await;
    let rack = at_bob.request().header::<RAck>().expect("an RAck").expect("readable RAck");
    assert_eq!(rack.rseq(), BOB_RSEQ, "the RAck reaches bob on the RSeq he stated");
    at_bob.respond(200, "OK").await;
    prack.expect(200).await;
    answer_and_hang_up(s, call, uas, record_route, "PRACK").await;
}

#[tokio::test(start_paused = true)]
async fn a_reliable_provisional_and_its_prack_cross_a_spiral() {
    reliable_provisional("spiral-prack", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_reliable_provisional_and_its_prack_cross_a_spiral_without_record_route() {
    reliable_provisional("spiral-no-rr-prack", RecordRoute::No).await;
}

// ── INFO ────────────────────────────────────────────────────────────────────

/// An INFO with a body crosses both calls, Content-Type and bytes intact.
async fn info(name: &str, record_route: RecordRoute, from: End) {
    const CT: &str = "application/example-binary";
    let body = b"spiral-info".to_vec();
    let mut c = Confirmed::new(name, record_route).await;
    let (dialog, other) = c.ends(from);
    let mut info =
        dialog.send_request(InDialogMethod::Info).with_body(CT, body.clone()).send().await;
    let mut uas = other.receive("INFO").await;
    assert_eq!(uas.request().raw(HeaderName::ContentType).next(), Some(CT));
    assert_eq!(&uas.request().body()[..], &body[..], "the INFO body crosses the spiral");
    uas.respond(200, "OK").await;
    info.expect(200).await;
    c.hang_up_and_finish(from, "INFO").await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_info_crosses_a_spiral() {
    info("spiral-info-caller", RecordRoute::Yes, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_info_crosses_a_spiral() {
    info("spiral-info-callee", RecordRoute::Yes, End::Callee).await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_info_crosses_a_spiral_without_record_route() {
    info("spiral-no-rr-info-caller", RecordRoute::No, End::Caller).await;
}

#[tokio::test(start_paused = true)]
async fn a_callee_info_crosses_a_spiral_without_record_route() {
    info("spiral-no-rr-info-callee", RecordRoute::No, End::Callee).await;
}
