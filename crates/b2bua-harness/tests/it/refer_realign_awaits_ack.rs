//! A blind transfer whose a-realign comes due while the transferee's ACK is
//! in flight (RFC 3261 §14.1).
//!
//! A re-INVITEs B, B answers 200 and the stack relays that 2xx to A; A's ACK
//! of it is in flight when B's REFER reaches the stack, and C answers its
//! INVITE and the c-realign re-INVITE inside that window. The 2xx this stack
//! sent A awaits her ACK, so an INVITE transaction is in progress on her
//! dialog: the a-realign re-INVITE waits for her ACK and leaves on it, carrying
//! C's description, and the transfer completes. A repeat of her ACK releases
//! nothing more. Where her ACK never comes, the a-realign watchdog rolls the
//! transfer back; a C whose c-realign 2xx offered gets its held ACK answering
//! that offer rejecting every stream ahead of the rollback's BYE.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;

use crate::common::sdp::apart_from_origin;
use scenario_harness::{Agent, Dialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_net_testkit::{RecordedSipEntry, SendDelay};

/// A's offer in her INVITE, where she makes one.
const ALICE_OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// B's answer to [`ALICE_OFFER`].
const BOB_ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// B's offer in the 2xx to A's offerless INVITE.
const BOB_OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's answer to [`BOB_OFFER`], in her ACK.
const ALICE_ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's offer in her re-INVITE, her session one version up.
const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// B's answer to [`ALICE_REOFFER`], his session one version up.
const BOB_REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's answer in the 2xx to the INVITE that dials it with A's offer.
const CHARLIE_FIRST_ANSWER: &str = "v=0\r\no=charlie 9 8 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's offer in the 2xx to the offerless INVITE that dials it.
const CHARLIE_FIRST_OFFER: &str = "v=0\r\no=charlie 9 8 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's active answer to the c-realign re-INVITE carrying A's offer.
const CHARLIE_REALIGN_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's offer in the 2xx to the offerless c-realign re-INVITE.
const CHARLIE_REALIGN_OFFER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's answer to the a-realign re-INVITE, her session one version past her
/// re-offer.
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// RFC 3261's T1: the first rung of the stack's 2xx ladder toward A
/// (§13.3.1.4).
const T1: Duration = Duration::from_millis(500);

/// How long the fabric holds A's ACK of her re-INVITE's 2xx: past the REFER,
/// C's INVITE and the c-realign round trips on the harness's 1 ms hops, inside
/// T1, so the stack re-sends no 2xx meanwhile.
const ACK_HELD: Duration = Duration::from_millis(100);

/// How long the fabric holds A's first ACK when she repeats it: past T1, so
/// the stack's 2xx copy draws her second ACK while the first is in flight.
const FIRST_ACK_HELD_PAST_T1: Duration = Duration::from_millis(600);

/// How long the fabric holds A's second ACK: it arrives after the first has
/// released the a-realign, while that re-INVITE awaits A's final.
const REPEAT_HELD: Duration = Duration::from_millis(200);

/// Past the transfer's round trips on the harness's 1 ms hops, inside
/// [`ACK_HELD`].
const PAST_THE_TRANSFERS_ROUND_TRIPS: Duration = Duration::from_millis(20);

/// The a-realign watchdog where C's c-realign 2xx offers: 60·T1, four T1
/// inside C's 64·T1 give-up on that 2xx (RFC 3261 §13.3.1.4), and inside the
/// stack's own 64·T1 wait for A's ACK.
const HOLD_BOUND: Duration = Duration::from_secs(30);

/// The defect the one-INVITE guard reports for a refused re-INVITE (ADR-0064).
const DEFECT: &str = "one-invite-in-progress-per-dialog";

/// Who makes the first offer of the A↔B call.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FirstOffer {
    /// A's INVITE carries it: the c-realign re-INVITE re-offers it and C
    /// answers in its 2xx.
    Transferee,
    /// A's INVITE carries none: the c-realign re-INVITE carries none either,
    /// C offers in its 2xx and the ACK of that 2xx waits for A's answer.
    Callee,
}

/// The parties once C has answered the c-realign re-INVITE, with A's ACK of
/// her re-INVITE's 2xx not yet at the stack.
struct ARealignDue {
    h: Harness,
    b2bua: B2buaSut,
    alice: Agent,
    bob: Agent,
    charlie: Agent,
    alice_dialog: Dialog,
    c_realign: ServerTxn,
    /// The CSeq number of A's re-INVITE, which her ACK carries.
    reinvite_cseq: u32,
}

impl ARealignDue {
    /// Every datagram `from` sent `to` whose start line begins `start`, on
    /// CSeq number `cseq` when one is given, first sends only (no re-emitted
    /// copy), with its place on the wire.
    fn sent(
        &self,
        from: SocketAddr,
        to: SocketAddr,
        start: &str,
        cseq: Option<u32>,
    ) -> Vec<(usize, RecordedSipEntry)> {
        self.h
            .wire_entries()
            .into_iter()
            .enumerate()
            .filter(|(_, e)| e.from == from && e.to == to && e.reemit.is_none())
            .filter(|(_, e)| e.raw.starts_with(start.as_bytes()))
            .filter(|(_, e)| {
                cseq.is_none_or(|n| sip_message::sniff::cseq_number(&e.raw) == Some(n))
            })
            .collect()
    }

    /// Every INVITE the stack sent A: the a-realign re-INVITEs.
    fn invites_to_alice(&self) -> Vec<(usize, RecordedSipEntry)> {
        self.sent(self.b2bua.addr, self.alice.addr(), "INVITE ", None)
    }

    /// Every ACK A sent of her re-INVITE's 2xx.
    fn alices_acks(&self) -> Vec<(usize, RecordedSipEntry)> {
        self.sent(self.alice.addr(), self.b2bua.addr, "ACK ", Some(self.reinvite_cseq))
    }

    /// Every ACK the stack sent C on the c-realign re-INVITE's CSeq.
    fn c_realign_acks(&self) -> Vec<(usize, RecordedSipEntry)> {
        let cseq = Some(self.c_realign.request().cseq().seq());
        self.sent(self.b2bua.addr, self.charlie.addr(), "ACK ", cseq)
    }

    /// No re-INVITE was refused so far: the one-INVITE guard reported nothing.
    fn assert_no_refusal(&mut self) {
        let refused: Vec<String> = self
            .b2bua
            .take_model_violations()
            .iter()
            .map(|v| v.lines().next().unwrap_or_default().to_string())
            .collect();
        assert!(
            refused.is_empty(),
            "the a-realign re-INVITE waits for A's ACK, never asked while her 2xx \
             awaits it ({DEFECT}): {refused:?}"
        );
    }
}

/// The fabric's hold on A's ACKs past the first (the ACK of her INVITE's 2xx,
/// never held): the n-th of them is held `holds[n]`, every later one the last
/// of `holds`; with `holds` empty, nothing is held.
fn alices_later_acks_held(alice: SocketAddr, holds: &'static [Duration]) -> SendDelay {
    let sent = AtomicUsize::new(0);
    Arc::new(move |src, _dst, datagram| {
        if src != alice || !datagram.starts_with(b"ACK ") {
            return None;
        }
        let n = sent.fetch_add(1, Ordering::Relaxed);
        let later = n.checked_sub(1)?;
        holds.get(later).or(holds.last()).copied()
    })
}

/// A INVITEs B, with the first offer as `first` has it, and the call is set
/// up. A re-INVITEs B with an offer, B answers 200 and A receives it; she
/// ACKs it when `alice_acks`, over a fabric holding her ACKs as `holds` says.
/// B REFERs A to C right after his 200; the stack dials C, C answers and B is
/// told the transfer succeeded; C answers the c-realign re-INVITE (and, where
/// it answered A's offer, receives its ACK at once). The stack runs its
/// production keepalive interval, past every scenario's span.
async fn a_realign_due(
    name: &str,
    n: u16,
    first: FirstOffer,
    holds: &'static [Duration],
    alice_acks: bool,
) -> ARealignDue {
    let (alice_port, bob_port, b2bua_port, charlie_port) = (5641 + n, 5651 + n, 5661 + n, 5671 + n);
    let alice_addr: SocketAddr = format!("127.0.0.1:{alice_port}").parse().expect("an address");
    let h = Harness::with_transit_delay_and_send_delay(
        name,
        1,
        alices_later_acks_held(alice_addr, holds),
    );
    let alice = h.agent("alice", &alice_addr.to_string()).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{charlie_port}")).await.transfer_target();
    let production = b2bua::B2buaConfig::default().keepalive_interval_sec;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
        .tune(move |c| c.keepalive_interval_sec = production)
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let (offer, bobs_sdp, alices_answer) = match first {
        FirstOffer::Transferee => (Some(ALICE_OFFER), BOB_ANSWER, None),
        FirstOffer::Callee => (None, BOB_OFFER, Some(ALICE_ANSWER)),
    };
    let invite = alice.invite(&bob).through(b2bua.addr);
    let invite = match offer {
        Some(sdp) => invite.with_sdp(sdp),
        None => invite,
    };
    let mut call = invite.send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(bobs_sdp).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack_with(alices_answer).await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut reinvite = alice_dialog.request(InDialogMethod::Invite, Some(ALICE_REOFFER)).await;
    let reinvite_cseq = reinvite.sent_invite().expect("A's re-INVITE").cseq().seq();
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(BOB_REANSWER).await;
    reinvite.expect(200).await;
    if alice_acks {
        alice_dialog.ack(None).await;
    }

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
    let first_sdp = match first {
        FirstOffer::Transferee => CHARLIE_FIRST_ANSWER,
        FirstOffer::Callee => CHARLIE_FIRST_OFFER,
    };
    charlie_uas.respond(200, "OK").with_sdp(first_sdp).await;
    charlie.receive("ACK").await;
    let mut success = bob.receive("NOTIFY").await;
    let sipfrag = String::from_utf8_lossy(success.request().body()).into_owned();
    assert!(sipfrag.starts_with("SIP/2.0 200"), "B is told the transfer succeeded: {sipfrag:?}");
    success.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    match first {
        FirstOffer::Transferee => {
            c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_ANSWER).await;
            charlie.receive("ACK").await;
        }
        FirstOffer::Callee => {
            c_realign.respond(200, "OK").with_sdp(CHARLIE_REALIGN_OFFER).await;
        }
    }
    ARealignDue { h, b2bua, alice, bob, charlie, alice_dialog, c_realign, reinvite_cseq }
}

/// The call's CDR events, as `(type, reason)` in the order written.
fn cdr_events(b2bua: &B2buaSut) -> Vec<(CdrEventType, Option<String>)> {
    b2bua
        .cdr_records()
        .iter()
        .flat_map(|r| r.events.iter().map(|e| (e.event_type, e.reason.clone())))
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

/// A receives the a-realign re-INVITE, carrying C's description of the
/// c-realign 2xx (its answer, or its offer as `first` has it), and answers
/// it: the stack ACKs her, ACKs C carrying her answer where C offered,
/// releases B and the transfer completes. A's BYE then ends the call on C,
/// and the call reaps fully.
async fn transfer_completes(mut t: ARealignDue, mut a_realign: ServerTxn, first: FirstOffer) {
    let c_description = match first {
        FirstOffer::Transferee => CHARLIE_REALIGN_ANSWER,
        FirstOffer::Callee => CHARLIE_REALIGN_OFFER,
    };
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(a_realign.request().body())),
        apart_from_origin(c_description),
        "the a-realign re-INVITE carries C's description of its c-realign 2xx"
    );
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("ACK").await;
    if first == FirstOffer::Callee {
        let ack = t.charlie.receive("ACK").await;
        assert_eq!(
            apart_from_origin(&String::from_utf8_lossy(ack.request().body())),
            apart_from_origin(ALICE_REALIGN_ANSWER),
            "C's 2xx offered: its ACK carries A's answer (RFC 3261 §13.2.2.4, RFC 3725 §4.1)"
        );
        assert_eq!(t.c_realign_acks().len(), 1, "one ACK of C's c-realign 2xx");
    }
    t.bob.receive("BYE").await.respond(200, "OK").await;
    let mut alice_bye = t.alice_dialog.bye().await;
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    t.assert_no_refusal();
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    let events = cdr_events(&t.b2bua);
    assert!(
        events.iter().any(|(_, r)| r.as_deref() == Some("transfer-completed")),
        "the transfer completes: {events:?}"
    );
    let _ = t.h.finish().await;
}

/// A's ACK of her re-INVITE's 2xx is in flight while B REFERs, C answers
/// its INVITE and the c-realign re-INVITE: the a-realign re-INVITE does not
/// leave while the stack's 2xx to A awaits that ACK, and leaves once it
/// arrives, the ACK relayed to B; the transfer completes. Under `first`.
async fn the_a_realign_leaves_on_the_transferees_ack(name: &str, n: u16, first: FirstOffer) {
    let mut t = a_realign_due(name, n, first, &[ACK_HELD], true).await;

    t.h.advance(PAST_THE_TRANSFERS_ROUND_TRIPS).await;
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before A's ACK arrives");
    let held_for_a = usize::from(first == FirstOffer::Transferee);
    assert_eq!(t.c_realign_acks().len(), held_for_a, "C's ACK waits for A's answer when C offers");

    t.bob.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    let acks = t.alices_acks();
    let invites = t.invites_to_alice();
    assert_eq!((acks.len(), invites.len()), (1, 1), "one ACK of A's, one a-realign re-INVITE");
    let arrived_ms = acks[0].1.sent_ms + ACK_HELD.as_millis() as u64;
    assert!(
        invites[0].1.sent_ms >= arrived_ms,
        "the a-realign re-INVITE leaves on A's ACK: sent at {} ms, the ACK arrived at {arrived_ms} ms",
        invites[0].1.sent_ms
    );
    transfer_completes(t, a_realign, first).await;
}

/// [`the_a_realign_leaves_on_the_transferees_ack`] where A's INVITE made the
/// first offer: C answers it in the c-realign 2xx, ACKed bare at once.
#[tokio::test(start_paused = true)]
async fn the_a_realign_leaves_on_the_transferees_ack_c_answering() {
    let name = "refer-realign-awaits-ack";
    the_a_realign_leaves_on_the_transferees_ack(name, 0, FirstOffer::Transferee).await;
}

/// [`the_a_realign_leaves_on_the_transferees_ack`] where B made the first
/// offer: C offers in the c-realign 2xx, the parked a-realign re-INVITE
/// carries that offer, and C's ACK, held for A's answer, carries it.
#[tokio::test(start_paused = true)]
async fn the_a_realign_leaves_on_the_transferees_ack_c_offering() {
    let name = "refer-realign-awaits-ack-c-offers";
    the_a_realign_leaves_on_the_transferees_ack(name, 3, FirstOffer::Callee).await;
}

/// The fabric holds A's ACK past T1: the stack re-sends its 2xx, A ACKs the
/// copy too, and her two ACKs arrive in turn. The first releases the
/// a-realign re-INVITE; the second, arriving while that re-INVITE awaits A's
/// final, releases nothing more.
#[tokio::test(start_paused = true)]
async fn a_repeated_ack_releases_the_a_realign_once() {
    let mut t = a_realign_due(
        "refer-realign-awaits-ack-repeat",
        1,
        FirstOffer::Transferee,
        &[FIRST_ACK_HELD_PAST_T1, REPEAT_HELD],
        true,
    )
    .await;

    t.h.advance(T1).await;
    t.alice.drain_tu().await;
    assert_eq!(t.alices_acks().len(), 2, "A ACKs the stack's 2xx copy as well");
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before A's ACK arrives");

    t.bob.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    t.h.advance(REPEAT_HELD).await;
    t.assert_no_refusal();
    assert_eq!(t.invites_to_alice().len(), 1, "A's repeated ACK releases no second re-INVITE");
    t.bob.drain().await;
    transfer_completes(t, a_realign, FirstOffer::Transferee).await;
}

/// A's ACK of her re-INVITE's 2xx never comes, and C's c-realign 2xx offers:
/// the a-realign re-INVITE never leaves, and C's ACK waits for A's answer.
/// The watchdog fires [`HOLD_BOUND`] after C's 2xx: C's ACK answers its offer
/// rejecting every stream, ahead of the rollback's BYE to C, and the rollback
/// BYEs A and B.
#[tokio::test(start_paused = true)]
async fn the_watchdog_ends_an_a_realign_still_waiting_for_the_transferees_ack() {
    let mut t =
        a_realign_due("refer-realign-awaits-ack-watchdog", 2, FirstOffer::Callee, &[], false).await;

    t.h.advance(HOLD_BOUND - Duration::from_millis(500)).await;
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE without A's ACK");
    assert!(t.c_realign_acks().is_empty(), "C's ACK still waits for A's answer");
    t.alice.drain().await;
    t.bob.drain().await;

    t.h.advance(Duration::from_secs(1)).await;
    let ack = t.charlie.receive("ACK").await;
    let body = String::from_utf8_lossy(ack.request().body()).into_owned();
    assert!(
        rejects_every_stream(&body),
        "with A's answer never coming, C's ACK answers its offer rejecting every stream \
         (RFC 3261 §13.2.2.4, RFC 3264 §6): {body:?}"
    );
    t.charlie.receive("BYE").await.respond(200, "OK").await;
    let acks = t.c_realign_acks();
    let byes = t.sent(t.b2bua.addr, t.charlie.addr(), "BYE ", None);
    assert_eq!(acks.len(), 1, "one ACK of C's c-realign 2xx");
    assert!(
        byes.first().is_some_and(|(bye, _)| acks[0].0 < *bye),
        "C's ACK leaves before the rollback's BYE"
    );
    t.alice.receive("BYE").await.respond(200, "OK").await;
    t.bob.receive("ACK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;

    t.assert_no_refusal();
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    assert!(t.invites_to_alice().is_empty(), "the a-realign re-INVITE never left");
    let events = cdr_events(&t.b2bua);
    assert!(
        events.iter().any(|(e, r)| *e == CdrEventType::Timeout
            && r.as_deref() == Some("transfer-rollback-a-realign")),
        "the watchdog rolls the transfer back: {events:?}"
    );
    t.alice.drain().await;
    let _ = t.h.finish().await;
}
