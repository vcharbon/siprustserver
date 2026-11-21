//! A blind transfer whose transferee made no offer (RFC 3261 §13.2.1).
//!
//! The stack re-INVITEs the transfer target C onto the transferee A's session
//! with no body, since A's INVITE carried none: C offers in its 2xx, and the ACK
//! of that 2xx MUST carry the answer (RFC 3261 §13.2.2.4). The answer is A's:
//! the stack re-INVITEs A with C's offer, and the ACK to C waits for A's 2xx and
//! carries her answer (RFC 3725 §4.1, flow I). While it waits, C's re-sent 2xx
//! draws no ACK. When A's answer never comes (she refuses, or never answers),
//! the ACK answers C's offer rejecting every stream (RFC 3264 §6) ahead of the
//! rollback's BYE; it never leaves bare.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;

use crate::common::sdp::apart_from_origin;
use scenario_harness::{Agent, Dialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;

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

/// The a-realign watchdog the timeout scenario runs on: well before C's
/// 64·T1 (32 s) give-up on its unacknowledged 2xx (RFC 3261 §13.3.1.4).
const SHORT_WATCHDOG_SEC: i64 = 10;

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
    fn c_realign_acks(&self) -> Vec<(usize, Vec<u8>)> {
        sent(&self.h, self.b2bua.addr, self.charlie.addr(), "ACK ", Some(self.c_cseq()))
    }

    /// Where on the wire A's first 2xx to the a-realign re-INVITE left.
    fn alice_answer_at(&self) -> usize {
        let cseq = Some(self.a_realign.request().cseq().seq());
        let answers = sent(&self.h, self.alice.addr(), self.b2bua.addr, "SIP/2.0 200", cseq);
        answers.first().expect("A answered the a-realign re-INVITE").0
    }
}

/// A INVITEs B with no offer, B offers in its 2xx and A answers in her ACK.
/// B REFERs A to C; the stack dials C with no offer, C offers in its 2xx and
/// is told nothing more is pending; the stack re-INVITEs C with no body, C
/// offers again in its 2xx, and A receives the re-INVITE carrying that offer.
/// `watchdog_sec` sets the realign re-INVITEs' answer watchdog, the default
/// when `None`.
async fn realigning(name: &str, n: u16, watchdog_sec: Option<i64>) -> Realigning {
    let (alice_port, bob_port, b2bua_port, charlie_port) = (5541 + n, 5551 + n, 5561 + n, 5571 + n);
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{charlie_port}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
        .tune(move |c| {
            if let Some(sec) = watchdog_sec {
                c.refer_reinvite_answer_sec = sec;
            }
        })
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

    let mut c_realign = charlie.receive("INVITE").await;
    assert!(
        c_realign.request().body().is_empty(),
        "A's INVITE made no offer: the c-realign re-INVITE carries none, C makes it"
    );
    c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;

    let a_realign = alice.receive("INVITE").await;
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(a_realign.request().body())),
        apart_from_origin(CHARLIE_REALIGN_OFFER),
        "the a-realign re-INVITE carries C's offer"
    );
    Realigning { h, b2bua, alice, bob, charlie, alice_dialog, c_realign, a_realign }
}

/// Every datagram `from` sent `to` whose start line begins `start`, on CSeq
/// number `cseq` when one is given, with its place on the wire.
fn sent(
    h: &Harness,
    from: SocketAddr,
    to: SocketAddr,
    start: &str,
    cseq: Option<u32>,
) -> Vec<(usize, Vec<u8>)> {
    h.wire_entries()
        .into_iter()
        .enumerate()
        .filter(|(_, e)| e.from == from && e.to == to && e.raw.starts_with(start.as_bytes()))
        .filter(|(_, e)| cseq.is_none_or(|n| sip_message::sniff::cseq_number(&e.raw) == Some(n)))
        .map(|(at, e)| (at, e.raw))
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

/// The rollback after A's answer can never come: C's held ACK answers its
/// offer rejecting every stream, on the wire before C's BYE; A, B and C are
/// BYE'd and the call reaps. `tolerated` are the requests A may still see
/// re-sent ahead of her BYE.
async fn rolled_back(t: Realigning, tolerated: &[&str]) -> B2buaSut {
    let ack = t.charlie.receive("ACK").await;
    let body = String::from_utf8_lossy(ack.request().body()).into_owned();
    assert!(
        rejects_every_stream(&body),
        "with A's answer never coming, C's ACK answers its offer rejecting every stream \
         (RFC 3261 §13.2.2.4, RFC 3264 §6), never bare: {body:?}"
    );
    t.alice.receive_tolerating("BYE", tolerated).await.respond(200, "OK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;

    let acks = t.c_realign_acks();
    let bye_at = sent(&t.h, t.b2bua.addr, t.charlie.addr(), "BYE ", None);
    assert_eq!(acks.len(), 1, "one ACK of C's c-realign 2xx");
    assert!(
        bye_at.first().is_some_and(|(bye, _)| acks[0].0 < *bye),
        "C's ACK leaves before the rollback's BYE"
    );

    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
    t.b2bua
}

/// The flow-I ladder: C's c-realign 2xx offers, A answers that offer in her
/// 2xx to the a-realign re-INVITE, and the ACK to C leaves after her 2xx
/// carrying her answer, restated on C's dialog (RFC 3725 §4.1). The transfer
/// completes; A's BYE ends the call.
#[tokio::test(start_paused = true)]
async fn the_ack_to_the_targets_offer_carries_the_transferees_answer() {
    let mut t = realigning("refer-delayed-offer-realign-answered", 0, None).await;

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

    t.bob.receive("BYE").await.respond(200, "OK").await;
    let mut alice_bye = t.alice_dialog.bye().await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
    let events = cdr_events(&t.b2bua);
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
    let mut t = realigning("refer-delayed-offer-realign-2xx-copies", 1, None).await;

    for _ in 0..3 {
        t.h.advance(Duration::from_millis(600)).await;
    }
    assert_eq!(
        t.c_realign_acks().len(),
        0,
        "C's 2xx and its copies draw no ACK before A's answer: no bare or rejecting ACK leaves"
    );
    let sent_2xx = sent(&t.h, t.charlie.addr(), t.b2bua.addr, "SIP/2.0 200", Some(t.c_cseq()));
    assert!(sent_2xx.len() >= 3, "C re-sent its 2xx on its ladder: {} sent", sent_2xx.len());
    t.alice.drain().await;

    t.a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("ACK").await;
    let ack = t.charlie.receive("ACK").await;
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(ack.request().body())),
        apart_from_origin(ALICE_REALIGN_ANSWER),
        "the ACK released by A's answer carries it"
    );

    t.c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;
    t.charlie.receive("ACK").await;
    let acks: Vec<Vec<u8>> = t.c_realign_acks().into_iter().map(|(_, raw)| raw).collect();
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
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let _ = t.h.finish().await;
}

/// A refuses the a-realign re-INVITE (488): her answer will not come, so C's
/// ACK answers its offer rejecting every stream, then the rollback BYEs A, B
/// and C.
#[tokio::test(start_paused = true)]
async fn the_transferees_refusal_releases_a_rejecting_ack_before_the_rollback() {
    let mut t = realigning("refer-delayed-offer-realign-488", 2, None).await;

    t.a_realign.respond(488, "Not Acceptable Here").await;
    t.alice.receive("ACK").await;
    let b2bua = rolled_back(t, &[]).await;
    let events = cdr_events(&b2bua);
    assert!(
        events.iter().any(|(e, r)| *e == CdrEventType::Reject
            && r.as_deref() == Some("transfer-rollback-a-realign")),
        "A's refusal rolls the transfer back: {events:?}"
    );
}

/// A never answers the a-realign re-INVITE: its watchdog fires (here well
/// before C's 64·T1 give-up), C's ACK answers its offer rejecting every
/// stream, then the rollback BYEs A, B and C.
#[tokio::test(start_paused = true)]
async fn the_watchdog_releases_a_rejecting_ack_before_the_rollback() {
    let t = realigning("refer-delayed-offer-realign-watchdog", 3, Some(SHORT_WATCHDOG_SEC)).await;

    let past_the_watchdog = Duration::from_secs(SHORT_WATCHDOG_SEC as u64 + 1);
    t.h.advance(past_the_watchdog).await;
    let b2bua = rolled_back(t, &["INVITE", "CANCEL"]).await;
    let events = cdr_events(&b2bua);
    assert!(
        events.iter().any(|(e, r)| *e == CdrEventType::Timeout
            && r.as_deref() == Some("transfer-rollback-a-realign")),
        "the watchdog rolls the transfer back: {events:?}"
    );
}
