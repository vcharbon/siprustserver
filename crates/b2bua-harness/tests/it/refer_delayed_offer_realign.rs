//! A blind transfer whose transferee made no offer (RFC 3261 §13.2.1).
//!
//! The stack re-INVITEs the transfer target C onto the transferee A's session
//! with no body, since A's INVITE carried none: C offers in its 2xx, and the ACK
//! of that 2xx MUST carry the answer (RFC 3261 §13.2.2.4). The answer is A's:
//! the stack re-INVITEs A with C's offer, and the ACK to C waits for A's 2xx and
//! carries her answer (RFC 3725 §4.1, flow I). While it waits, C's re-sent 2xx
//! draws no ACK. When A's answer never comes (she refuses, never answers, or
//! hangs up), the ACK answers C's offer rejecting every stream (RFC 3264 §6)
//! ahead of the teardown's BYE; it never leaves bare. The wait ends inside
//! C's 64·T1 window, so that ACK reaches C before C gives the 2xx up
//! (§13.3.1.4); a C that gives up first by its own BYE is owed nothing more.
//! Where A's INVITE did carry the offer, C answers in its 2xx: its ACK leaves
//! bare at once, and A's answer to the a-realign adds none.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;

use crate::common::sdp::apart_from_origin;
use scenario_harness::{Agent, Dialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_net_testkit::RecordedSipEntry;

/// B's offer in the 2xx to A's offerless INVITE.
const BOB_OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's answer to [`BOB_OFFER`], in her ACK.
const ALICE_ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's offer in the 2xx to the offerless INVITE that dials it.
const CHARLIE_FIRST_OFFER: &str = "v=0\r\no=charlie 9 8 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's offer in the 2xx to the offerless re-INVITE, its session one version up.
const CHARLIE_REALIGN_OFFER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's answer to [`CHARLIE_REALIGN_OFFER`], her session one version up.
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's offer in her INVITE, where she makes one.
const ALICE_OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// B's answer to [`ALICE_OFFER`] in its 2xx.
const BOB_ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's answer in its 2xx to the c-realign re-INVITE carrying A's offer.
const CHARLIE_REALIGN_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The longest the ACK of C's offering 2xx waits for A's answer: 60·T1, four
/// T1 inside C's 64·T1 (32 s) give-up on the unacknowledged 2xx (RFC 3261
/// §13.3.1.4), whatever the realign watchdog is configured to.
const HOLD_BOUND: Duration = Duration::from_secs(30);

/// C's give-up on its unacknowledged 2xx: 64·T1 (RFC 3261 §13.3.1.4).
const TARGET_GIVE_UP: Duration = Duration::from_secs(32);

/// RFC 3261's T1, the first rung of C's 2xx ladder (§13.3.1.4).
const T1: Duration = Duration::from_millis(500);

/// The parties of a transfer parked on the flow-I hold: C has sent the 2xx
/// that offers to the offerless re-INVITE, and A holds the re-INVITE that
/// carries that offer, unanswered.
struct Realigning {
    h: Harness,
    b2bua: B2buaSut,
    alice: Agent,
    bob: Agent,
    charlie: Agent,
    alice_dialog: Dialog,
    bob_dialog: Dialog,
    c_realign: ServerTxn,
    a_realign: ServerTxn,
}

impl Realigning {
    /// The c-realign re-INVITE's CSeq number on C's dialog.
    fn c_cseq(&self) -> u32 {
        self.c_realign.request().cseq().seq()
    }

    /// Every ACK the stack sent C on the c-realign re-INVITE's CSeq, with its
    /// place on the wire.
    fn c_realign_acks(&self) -> Vec<(usize, RecordedSipEntry)> {
        sent(&self.h, self.b2bua.addr, self.charlie.addr(), "ACK ", Some(self.c_cseq()))
    }

    /// Every 2xx C sent the stack to the c-realign re-INVITE, first and copies.
    fn c_realign_2xx(&self) -> Vec<(usize, RecordedSipEntry)> {
        sent(&self.h, self.charlie.addr(), self.b2bua.addr, "SIP/2.0 200", Some(self.c_cseq()))
    }

    /// Where on the wire A's first 2xx to the a-realign re-INVITE left.
    fn alice_answer_at(&self) -> usize {
        let cseq = Some(self.a_realign.request().cseq().seq());
        let answers = sent(&self.h, self.alice.addr(), self.b2bua.addr, "SIP/2.0 200", cseq);
        answers.first().expect("A answered the a-realign re-INVITE").0
    }

    /// The wire's length now: what is sent from here on lies at or past it.
    fn wire_mark(&self) -> usize {
        self.h.wire_entries().len()
    }
}

/// The parties of a transfer whose c-realign re-INVITE, carrying no offer,
/// C holds unanswered.
struct CRealigning {
    h: Harness,
    b2bua: B2buaSut,
    alice: Agent,
    bob: Agent,
    charlie: Agent,
    alice_dialog: Dialog,
    bob_dialog: Dialog,
    c_realign: ServerTxn,
}

/// [`c_realigning`], then C offers in its 2xx to the c-realign re-INVITE and
/// A receives the re-INVITE carrying that offer.
async fn realigning(name: &str, n: u16) -> Realigning {
    let CRealigning { h, b2bua, alice, bob, charlie, alice_dialog, bob_dialog, mut c_realign } =
        c_realigning(name, n).await;
    c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;

    let a_realign = alice.receive("INVITE").await;
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(a_realign.request().body())),
        apart_from_origin(CHARLIE_REALIGN_OFFER),
        "the a-realign re-INVITE carries C's offer"
    );
    Realigning { h, b2bua, alice, bob, charlie, alice_dialog, bob_dialog, c_realign, a_realign }
}

/// A INVITEs B with no offer, B offers in its 2xx and A answers in her ACK.
/// B REFERs A to C; the stack dials C with no offer, C offers in its 2xx and
/// B is told the transfer succeeded; C receives the c-realign re-INVITE, with
/// no body. The stack runs its default realign watchdog, and its production
/// keepalive interval, past every scenario's span.
async fn c_realigning(name: &str, n: u16) -> CRealigning {
    let (alice_port, bob_port, b2bua_port, charlie_port) = (5541 + n, 5551 + n, 5561 + n, 5571 + n);
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{charlie_port}")).await.transfer_target();
    let production = b2bua::B2buaConfig::default().keepalive_interval_sec;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
        .tune(move |c| c.keepalive_interval_sec = production)
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_OFFER).await;
    call.expect(200).await;
    let alice_dialog = call.ack_with(Some(ALICE_ANSWER)).await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let x_api_call = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{charlie_port}}}}}"#
    );
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{charlie_port}>"))
        .with_header("X-Api-Call", &x_api_call)
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_FIRST_OFFER).await;
    let first_ack = charlie.receive("ACK").await;
    assert!(
        !first_ack.request().body().is_empty(),
        "the ACK of C's first 2xx answers its offer (RFC 3261 §13.2.2.4)"
    );
    let mut success = bob.receive("NOTIFY").await;
    let sipfrag = String::from_utf8_lossy(success.request().body()).into_owned();
    assert!(sipfrag.starts_with("SIP/2.0 200"), "B is told the transfer succeeded: {sipfrag:?}");
    success.respond(200, "OK").await;

    let c_realign = charlie.receive("INVITE").await;
    assert!(
        c_realign.request().body().is_empty(),
        "A's INVITE made no offer: the c-realign re-INVITE carries none, C makes it"
    );
    CRealigning { h, b2bua, alice, bob, charlie, alice_dialog, bob_dialog, c_realign }
}

/// Every datagram `from` sent `to` whose start line begins `start`, on CSeq
/// number `cseq` when one is given, with its place on the wire.
fn sent(
    h: &Harness,
    from: SocketAddr,
    to: SocketAddr,
    start: &str,
    cseq: Option<u32>,
) -> Vec<(usize, RecordedSipEntry)> {
    h.wire_entries()
        .into_iter()
        .enumerate()
        .filter(|(_, e)| e.from == from && e.to == to && e.raw.starts_with(start.as_bytes()))
        .filter(|(_, e)| cseq.is_none_or(|n| sip_message::sniff::cseq_number(&e.raw) == Some(n)))
        .collect()
}

/// Whether `sdp` lists media, every stream of it at port 0 (RFC 3264 §6).
fn rejects_every_stream(sdp: &str) -> bool {
    let ports: Vec<&str> = sdp
        .lines()
        .filter_map(|l| l.strip_prefix("m="))
        .map(|m| m.split(' ').nth(1).unwrap_or_default())
        .collect();
    !ports.is_empty() && ports.iter().all(|p| *p == "0")
}

/// The call's CDR events, as `(type, reason)` in the order written.
fn cdr_events(b2bua: &B2buaSut) -> Vec<(CdrEventType, Option<String>)> {
    b2bua
        .cdr_records()
        .iter()
        .flat_map(|r| r.events.iter().map(|e| (e.event_type, e.reason.clone())))
        .collect()
}

/// C's release once A's answer can never come: C's held ACK answers its offer
/// rejecting every stream, on the wire at or past `held_until` (the wire's
/// length when the answer stopped coming) and before the teardown's BYE to C,
/// which C answers. Returns that ACK as it left.
async fn charlie_released(t: &Realigning, held_until: usize) -> RecordedSipEntry {
    let ack = t.charlie.receive("ACK").await;
    let body = String::from_utf8_lossy(ack.request().body()).into_owned();
    assert!(
        rejects_every_stream(&body),
        "with A's answer never coming, C's ACK answers its offer rejecting every stream \
         (RFC 3261 §13.2.2.4, RFC 3264 §6), never bare: {body:?}"
    );
    t.charlie.receive("BYE").await.respond(200, "OK").await;

    let mut acks = t.c_realign_acks();
    let bye_at = sent(&t.h, t.b2bua.addr, t.charlie.addr(), "BYE ", None);
    assert_eq!(acks.len(), 1, "one ACK of C's c-realign 2xx");
    assert!(
        acks[0].0 >= held_until,
        "C's ACK waits for A's answer: it leaves only once that answer cannot come"
    );
    assert!(
        bye_at.first().is_some_and(|(bye, _)| acks[0].0 < *bye),
        "C's ACK leaves before the teardown's BYE"
    );
    acks.remove(0).1
}

/// The rollback after A's answer can never come: C is released
/// ([`charlie_released`]), A and B are BYE'd and the call reaps. `tolerated`
/// are the requests A may still see re-sent ahead of her BYE.
async fn rolled_back(t: Realigning, held_until: usize, tolerated: &[&str]) -> B2buaSut {
    charlie_released(&t, held_until).await;
    t.alice.receive_tolerating("BYE", tolerated).await.respond(200, "OK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    reaped(t).await
}

/// The call reaps fully and the wire audit closes the run.
async fn reaped(t: Realigning) -> B2buaSut {
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
    t.b2bua
}

/// A answers the a-realign re-INVITE: the ACK to C leaves after her 2xx
/// carrying her answer, the one ACK of C's c-realign 2xx.
async fn alice_answers(t: &mut Realigning) {
    t.a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("ACK").await;
    let ack = t.charlie.receive("ACK").await;
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(ack.request().body())),
        apart_from_origin(ALICE_REALIGN_ANSWER),
        "C's 2xx offered: its ACK carries A's answer (RFC 3261 §13.2.2.4, RFC 3725 §4.1)"
    );
    let acks = t.c_realign_acks();
    assert_eq!(acks.len(), 1, "one ACK of C's c-realign 2xx");
    assert!(acks[0].0 > t.alice_answer_at(), "C's ACK leaves after A's 2xx carrying the answer");
}

/// The flow-I ladder: C's c-realign 2xx offers, A answers that offer in her
/// 2xx to the a-realign re-INVITE, and the ACK to C leaves after her 2xx
/// carrying her answer, restated on C's dialog (RFC 3725 §4.1). The transfer
/// completes; A's BYE ends the call.
#[tokio::test(start_paused = true)]
async fn the_ack_to_the_targets_offer_carries_the_transferees_answer() {
    let mut t = realigning("refer-delayed-offer-realign-answered", 0).await;

    alice_answers(&mut t).await;

    t.bob.receive("BYE").await.respond(200, "OK").await;
    let mut alice_bye = t.alice_dialog.bye().await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let b2bua = reaped(t).await;
    let events = cdr_events(&b2bua);
    assert!(
        events.iter().any(|(_, r)| r.as_deref() == Some("transfer-completed")),
        "the transfer completes: {events:?}"
    );
    assert!(events.iter().any(|(e, _)| *e == CdrEventType::Bye), "A's BYE: {events:?}");
}

/// C re-sends its c-realign 2xx on its §13.3.1.4 ladder while the ACK waits
/// for A's answer: the copies draw no ACK. A's answer releases the one ACK,
/// carrying it, and a later copy draws that ACK again, byte for byte
/// (§13.2.2.4).
#[tokio::test(start_paused = true)]
async fn the_targets_resent_2xx_draws_no_ack_while_the_answer_is_awaited() {
    let mut t = realigning("refer-delayed-offer-realign-2xx-copies", 1).await;

    for rung in [T1, 2 * T1, 4 * T1] {
        t.h.advance(rung).await;
        t.c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;
    }
    t.h.advance(T1).await;
    assert_eq!(
        t.c_realign_acks().len(),
        0,
        "C's 2xx and its copies draw no ACK before A's answer: no bare or rejecting ACK leaves"
    );
    assert_eq!(t.c_realign_2xx().len(), 4, "C sent its 2xx and three copies on its ladder");
    t.alice.drain().await;

    alice_answers(&mut t).await;

    t.c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;
    t.charlie.receive("ACK").await;
    let acks: Vec<Vec<u8>> = t.c_realign_acks().into_iter().map(|(_, e)| e.raw).collect();
    assert_eq!(acks.len(), 2, "one ACK, then one per later copy of C's 2xx");
    assert_eq!(
        String::from_utf8_lossy(&acks[1]),
        String::from_utf8_lossy(&acks[0]),
        "the copy is re-ACKed with the ACK its 2xx drew (RFC 3261 §13.2.2.4)"
    );

    t.bob.receive("BYE").await.respond(200, "OK").await;
    let mut alice_bye = t.alice_dialog.bye().await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    reaped(t).await;
}

/// A refuses the a-realign re-INVITE (488): her answer will not come, so C's
/// ACK answers its offer rejecting every stream, then the rollback BYEs A, B
/// and C.
#[tokio::test(start_paused = true)]
async fn the_transferees_refusal_releases_a_rejecting_ack_before_the_rollback() {
    let mut t = realigning("refer-delayed-offer-realign-488", 2).await;

    let refused_at = t.wire_mark();
    t.a_realign.respond(488, "Not Acceptable Here").await;
    t.alice.receive("ACK").await;
    let b2bua = rolled_back(t, refused_at, &[]).await;
    let events = cdr_events(&b2bua);
    assert!(
        events.iter().any(|(e, r)| *e == CdrEventType::Reject
            && r.as_deref() == Some("transfer-rollback-a-realign")),
        "A's refusal rolls the transfer back: {events:?}"
    );
}

/// A never answers the a-realign re-INVITE. The default watchdog (32 s) would
/// meet C's 64·T1 give-up; while C's ACK waits it fires [`HOLD_BOUND`] after
/// C's 2xx arrived, so C's ACK answers its offer rejecting every stream inside
/// C's window, then the rollback BYEs A, B and C.
#[tokio::test(start_paused = true)]
async fn the_watchdog_releases_a_rejecting_ack_inside_the_targets_2xx_window() {
    let t = realigning("refer-delayed-offer-realign-watchdog", 3).await;

    t.h.advance(HOLD_BOUND - Duration::from_millis(500)).await;
    assert!(t.c_realign_acks().is_empty(), "C's ACK still waits for A's answer");
    let expired_at = t.wire_mark();
    t.h.advance(Duration::from_secs(1)).await;
    let ack = charlie_released(&t, expired_at).await;
    let first_2xx_ms = t.c_realign_2xx()[0].1.sent_ms;
    let held = Duration::from_millis(ack.sent_ms - first_2xx_ms);
    assert!(
        held <= HOLD_BOUND + Duration::from_millis(1),
        "C's ACK leaves at most 60·T1 after its 2xx arrived (1 ms transit), \
         before C's {TARGET_GIVE_UP:?} give-up: {held:?}"
    );
    t.alice.receive_tolerating("BYE", &["INVITE", "CANCEL"]).await.respond(200, "OK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    let b2bua = reaped(t).await;
    let events = cdr_events(&b2bua);
    assert!(
        events.iter().any(|(e, r)| *e == CdrEventType::Timeout
            && r.as_deref() == Some("transfer-rollback-a-realign")),
        "the watchdog rolls the transfer back: {events:?}"
    );
}

/// A hangs up while C's ACK waits for her answer, and answers the re-INVITE
/// still pending at her with 487: her answer will not come, so C's ACK
/// answers its offer rejecting every stream ahead of the teardown's BYE to C.
#[tokio::test(start_paused = true)]
async fn the_transferees_bye_releases_a_rejecting_ack_before_the_teardown() {
    let mut t = realigning("refer-delayed-offer-realign-a-bye", 4).await;

    let hung_up_at = t.wire_mark();
    let mut alice_bye = t.alice_dialog.bye().await;
    alice_bye.expect(200).await;
    t.a_realign.respond(487, "Request Terminated").await;
    t.alice.receive("ACK").await;
    charlie_released(&t, hung_up_at).await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    let b2bua = reaped(t).await;
    let events = cdr_events(&b2bua);
    assert!(events.iter().any(|(e, _)| *e == CdrEventType::Bye), "A's BYE: {events:?}");
}

/// The referrer B hangs up while C's ACK waits for A's answer: B's BYE ends
/// B's leg alone (RFC 5589 §6.1), so A's answer can still come and C's ACK
/// goes on waiting; A's answer releases it, carrying the answer, and the
/// transfer completes.
#[tokio::test(start_paused = true)]
async fn the_referrers_bye_leaves_the_ack_waiting_for_the_transferees_answer() {
    let mut t = realigning("refer-delayed-offer-realign-b-bye", 5).await;

    let mut bob_bye = t.bob_dialog.bye().await;
    bob_bye.expect(200).await;
    assert!(t.c_realign_acks().is_empty(), "B's BYE releases no ACK: A's answer can still come");
    alice_answers(&mut t).await;

    let mut alice_bye = t.alice_dialog.bye().await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    let b2bua = reaped(t).await;
    let events = cdr_events(&b2bua);
    assert!(
        events.iter().any(|(_, r)| r.as_deref() == Some("transfer-completed")),
        "the transfer completes: {events:?}"
    );
}

/// C gives its unacknowledged 2xx up by its own BYE while the ACK waits for
/// A's answer (§13.3.1.4, a C whose give-up comes early): C's dialog ends with
/// nothing more owed to it, so no ACK of its c-realign 2xx ever leaves; the
/// teardown BYEs A, who answers the re-INVITE still pending at her with 487
/// (§15.1.2), and B.
#[tokio::test(start_paused = true)]
async fn the_targets_own_bye_leaves_it_owed_no_ack() {
    let mut t = realigning("refer-delayed-offer-realign-c-bye", 6).await;

    t.h.advance(Duration::from_secs(2)).await;
    let mut charlie_dialog = t.c_realign.dialog();
    let mut charlie_bye = charlie_dialog.bye().await;
    charlie_bye.expect(200).await;
    t.alice.receive_tolerating("BYE", &["INVITE"]).await.respond(200, "OK").await;
    t.a_realign.respond(487, "Request Terminated").await;
    t.alice.receive("ACK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| t.b2bua.is_reaped()).await;
    assert!(t.c_realign_acks().is_empty(), "C's own BYE leaves it owed no ACK, before or after");
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
}

/// C's own BYE ends the call while the ACK waits, and A's 2xx to the
/// a-realign re-INVITE crosses the teardown's BYE to her: the stack ACKs that
/// 2xx (RFC 3261 §13.2.2.4) though no transfer follows, and sends C nothing
/// more.
#[tokio::test(start_paused = true)]
async fn the_transferees_answer_crossing_the_teardown_is_acked() {
    let mut t = realigning("refer-delayed-offer-realign-crossed-answer", 7).await;

    let mut charlie_dialog = t.c_realign.dialog();
    let mut charlie_bye = charlie_dialog.bye().await;
    charlie_bye.expect(200).await;
    t.a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("BYE").await.respond(200, "OK").await;
    t.alice.receive("ACK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| t.b2bua.is_reaped()).await;
    assert!(t.c_realign_acks().is_empty(), "C's own BYE leaves it owed no ACK");
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
}

/// A hangs up while C holds the c-realign re-INVITE, and C's 2xx offering to
/// it crosses the teardown: its ACK answers the offer rejecting every stream
/// (RFC 3261 §13.2.2.4, RFC 3264 §6), since no answer of A's can come. The
/// teardown's own ACK does this, with no hold armed; the scenario pins that
/// the hold leaves it so.
#[tokio::test(start_paused = true)]
async fn the_targets_offer_crossing_the_teardown_is_answered_rejecting() {
    let CRealigning { h, b2bua, bob, charlie, mut alice_dialog, mut c_realign, .. } =
        c_realigning("refer-delayed-offer-realign-crossed-offer", 8).await;

    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;
    let ack = charlie.receive_tolerating("ACK", &["BYE", "CANCEL"]).await;
    let body = String::from_utf8_lossy(ack.request().body()).into_owned();
    assert!(
        rejects_every_stream(&body),
        "C's offer crossing the teardown is answered rejecting every stream, never bare: {body:?}"
    );
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A's INVITE carries the offer, so the c-realign re-INVITE re-offers it to
/// C and C answers in its 2xx: the ACK leaves bare at once, and a copy of
/// that 2xx draws it again (RFC 3261 §13.2.2.4). C's ACK owes no answer, so
/// A's 2xx to the a-realign re-INVITE sends C no further ACK.
#[tokio::test(start_paused = true)]
async fn a_transferees_offer_leaves_the_targets_ack_bare_and_unrepeated() {
    let n = 9u16;
    let (alice_port, bob_port, b2bua_port, charlie_port) = (5541 + n, 5551 + n, 5561 + n, 5571 + n);
    let h = Harness::with_transit_delay("refer-delayed-offer-realign-offerful", 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{charlie_port}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let x_api_call = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{charlie_port}}}}}"#
    );
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{charlie_port}>"))
        .with_header("X-Api-Call", &x_api_call)
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_FIRST_OFFER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    assert!(!c_realign.request().body().is_empty(), "the c-realign re-INVITE carries A's offer");
    let c_cseq = Some(c_realign.request().cseq().seq());
    c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_ANSWER).await;
    let ack = charlie.receive("ACK").await;
    assert!(ack.request().body().is_empty(), "C's 2xx answered: its ACK is bare");
    let mut a_realign = alice.receive("INVITE").await;
    h.advance(T1).await;
    c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_ANSWER).await;
    charlie.receive("ACK").await;

    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    let acks = sent(&h, b2bua.addr, charlie.addr(), "ACK ", c_cseq);
    assert_eq!(acks.len(), 2, "C's 2xx and its copy draw one ACK each; A's answer draws none");

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
