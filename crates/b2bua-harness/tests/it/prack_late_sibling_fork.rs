//! **RFC 3262 §4 — a sibling fork's first reliable provisional reaching this
//! stack after another fork's 2xx is PRACKed here.** A forked callee's 2xx on
//! one fork established the call; another fork's reliable 183, sent before
//! that 2xx, is held in flight and reaches this stack only afterwards. Its
//! early dialog lives on (RFC 3261 §13.2.2.4), and this stack, that fork's
//! UAC, is the only party that took the provisional: it PRACKs it once, on
//! that fork's own early dialog, and relays nothing toward the caller, whose
//! INVITE transaction already sent its final (RFC 3261 §13.3.1.1). The early
//! dialog then ends by the fork's own 2xx (ACKed and BYEd as any straggler) or
//! silently at the 64·T1 bound; the call keeps the winner. Under the
//! transparent relay and under fake-prack alike.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, stated, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Agent, ClientInvite, Dialog, Harness, ServerTxn};
use sip_net_testkit::{RecordedSipEntry, SendDelay};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The winning fork's own offer, on its 2xx to an offerless INVITE.
const WINNER_OFFER: &str = "v=0\r\no=bob 2 2 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// The caller's answer to [`WINNER_OFFER`], on her ACK.
const CALLER_ANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// The late fork's own offer, on its reliable 183.
const LATE_FORK_OFFER: &str = "v=0\r\no=bob 3 3 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";

/// The fork whose 183 rings before the answer.
const EARLY_FORK: &str = "fork1";
/// The fork whose 2xx establishes the call.
const WINNER: &str = "fork2";
/// The fork whose first reliable 183 arrives after the winner's 2xx.
const LATE_FORK: &str = "fork3";
const EARLY_FORK_RSEQ: u32 = 11;
const LATE_FORK_RSEQ: u32 = 31;

/// RFC 3261 T1: the interval of a reliable provisional's first
/// retransmission (RFC 3262 §3).
const T1: Duration = Duration::from_millis(500);

/// How long the fabric holds the late fork's 183 past its transit: past the
/// winner's 2xx, the caller's ACK and its relay on the harness's 1 ms hops,
/// well inside T1.
const HELD: Duration = Duration::from_millis(20);

/// How long the fabric holds the late fork's first 183 when the callee
/// repeats it: past its retransmission at T1, so the copy arrives first.
const HELD_PAST_T1: Duration = Duration::from_millis(600);

/// Past a round trip on the harness's 1 ms transit, well inside T1.
const PAST_A_ROUND_TRIP: Duration = Duration::from_millis(50);

/// Past 64·T1 (32 s) after the winner's 2xx: the bound of the late fork's
/// early dialog (RFC 3261 §13.2.2.4).
const PAST_THE_EARLY_DIALOG_BOUND: Duration = Duration::from_secs(33);

/// Who relays the callee's provisionals before the answer.
#[derive(Clone, Copy)]
enum Relay {
    /// The caller sees each reliable 183 and PRACKs it.
    Transparent,
    /// The caller sees a bare 180; this stack PRACKs the callee itself.
    FakePrack,
}

/// The callee has rung and not answered: the caller's INVITE, the callee's
/// INVITE server transaction, and the INVITE's CSeq.
struct Ringing {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
    call: ClientInvite,
    uas: ServerTxn,
    invite_cseq: u32,
}

/// The established call, the callee's INVITE server transaction, and the
/// INVITE's CSeq.
struct Answered {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
    uas: ServerTxn,
    dialog: Dialog,
    invite_cseq: u32,
}

/// Whether `datagram` is a 183 of [`LATE_FORK`]'s.
fn is_the_late_forks_183(datagram: &[u8]) -> bool {
    let text = String::from_utf8_lossy(datagram);
    text.starts_with("SIP/2.0 183 ") && text.contains(&format!(";tag={LATE_FORK}"))
}

/// The fabric's hold on [`LATE_FORK`]'s 183s: the n-th one sent is held
/// `holds[n]`, every later one the last of `holds`; nothing else is held.
fn the_late_forks_183_held(holds: &'static [Duration]) -> SendDelay {
    let sent = AtomicUsize::new(0);
    Arc::new(move |_src, _dst, datagram| {
        if !is_the_late_forks_183(datagram) {
            return None;
        }
        let n = sent.fetch_add(1, Ordering::Relaxed);
        holds.get(n).or(holds.last()).copied()
    })
}

/// A harness whose fabric holds the late fork's 183s as `holds` says, its
/// parties and this stack under `relay`, with the keepalive out of every
/// window these tests wait through.
async fn parties(
    name: &str,
    relay: Relay,
    holds: &'static [Duration],
) -> (Harness, Agent, Agent, B2buaSut) {
    let h = Harness::with_transit_delay_and_send_delay(name, 1, the_late_forks_183_held(holds));
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let port = bob.addr().port();
    let builder = match relay {
        Relay::Transparent => B2buaSut::route_all_to("127.0.0.1", port),
        Relay::FakePrack => {
            B2buaSut::route_all_to_with_18x("127.0.0.1", port, RelayFirst18xStrategy::FakePrack)
        }
    };
    let b2bua = builder.tune(|c| c.keepalive_interval_sec = 120).start(&h, "b2bua", &at).await;
    (h, alice, bob, b2bua)
}

/// The callee forks: [`EARLY_FORK`] rings with a reliable 183, PRACKed by
/// the caller or by this stack as `relay` has it.
async fn rung_by_an_early_fork(name: &str, relay: Relay, holds: &'static [Duration]) -> Ringing {
    let (h, alice, bob, b2bua) = parties(name, relay, holds).await;
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();
    uas.respond(183, "Session Progress")
        .with_to_tag(EARLY_FORK)
        .reliable(EARLY_FORK_RSEQ)
        .with_sdp(ANSWER)
        .await;
    match relay {
        Relay::Transparent => {
            let ring = call.expect(183).await;
            let (mut prack, _) =
                call.try_prack_with_request(&ring).await.expect("alice PRACKs the early fork");
            bob.receive("PRACK").await.respond(200, "OK").await;
            prack.expect(200).await;
        }
        Relay::FakePrack => {
            call.expect(180).await;
            bob.receive("PRACK").await.respond(200, "OK").await;
        }
    }
    Ringing { h, alice, bob, b2bua, call, uas, invite_cseq }
}

/// The callee does not ring before the late fork: an offerless INVITE
/// reaches him.
async fn offerless_invite(name: &str) -> Ringing {
    let (h, alice, bob, b2bua) = parties(name, Relay::Transparent, &[HELD]).await;
    let call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let uas = bob.receive("INVITE").await;
    assert!(uas.request().sdp().is_none(), "an offerless INVITE reaches the callee");
    let invite_cseq = uas.request().cseq().seq();
    Ringing { h, alice, bob, b2bua, call, uas, invite_cseq }
}

/// [`LATE_FORK`] sends its first reliable 183, with `RSeq`
/// [`LATE_FORK_RSEQ`] and `sdp`, which the fabric holds.
async fn late_fork_rings(r: &mut Ringing, sdp: &str) {
    r.uas
        .respond(183, "Session Progress")
        .with_to_tag(LATE_FORK)
        .reliable(LATE_FORK_RSEQ)
        .with_sdp(sdp)
        .await;
}

/// [`LATE_FORK`] retransmits its 183 at T1 (RFC 3262 §3): the same datagram
/// again, unPRACKed so far as the callee knows.
async fn late_fork_repeats_its_183(r: &Ringing) {
    r.h.advance(T1).await;
    let first =
        r.h.wire_entries()
            .into_iter()
            .find(|e| e.from == r.bob.addr() && is_the_late_forks_183(&e.raw))
            .expect("the late fork's 183");
    r.bob.try_send_datagram(&first.raw, r.b2bua.addr).await.expect("the copy leaves");
}

/// [`WINNER`] answers 200 with `sdp` and the caller ACKs, carrying `answer`
/// when the 200 offers.
async fn winner_answers(r: Ringing, sdp: &str, answer: Option<&str>) -> Answered {
    let Ringing { h, alice, bob, b2bua, mut call, mut uas, invite_cseq } = r;
    uas.respond(200, "OK").with_to_tag(WINNER).with_sdp(sdp).await;
    call.expect(200).await;
    let dialog = call.ack_with(answer).await;
    bob.receive("ACK").await;
    Answered { h, alice, bob, b2bua, uas, dialog, invite_cseq }
}

/// The PRACK this stack owes [`LATE_FORK`]: on that fork's early dialog,
/// naming its `RSeq` and the INVITE, first on that dialog's CSeq sequence.
async fn expect_the_late_forks_prack(a: &Answered) -> ServerTxn {
    let prack = a.bob.receive("PRACK").await;
    let req = prack.request();
    assert_eq!(req.to().tag(), Some(LATE_FORK), "the late fork's own early dialog");
    assert_eq!(
        stated(req, "RAck").as_deref(),
        Some(format!("{LATE_FORK_RSEQ} {} INVITE", a.invite_cseq).as_str()),
        "the PRACK names the late fork's provisional"
    );
    assert_eq!(req.cseq().seq(), a.invite_cseq + 1, "that dialog's first request");
    prack
}

/// The `(To-tag, RAck)` of each PRACK `from` first sent `to`, in order.
fn pracks_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.reemit.is_none())
        .filter(|e| e.raw.starts_with(b"PRACK "))
        .filter_map(|e| {
            let text = std::str::from_utf8(&e.raw).ok()?;
            let line = |name: &str| {
                text.split("\r\n")
                    .find(|l| l.len() > name.len() && l[..name.len()].eq_ignore_ascii_case(name))
                    .map(|l| l[name.len()..].trim().to_string())
            };
            let tag = line("To:")?.split(";tag=").nth(1)?.to_string();
            Some(format!("{tag}: {}", line("RAck:")?))
        })
        .collect()
}

/// [`LATE_FORK`] answers 200 too, repeating the `sdp` of its 183: this stack
/// ACKs and BYEs that straggler.
async fn late_fork_answers(a: &mut Answered, sdp: &str) {
    a.uas.respond(200, "OK").with_to_tag(LATE_FORK).with_sdp(sdp).await;
    let ack = a.bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some(LATE_FORK), "the straggler's 2xx is ACKed");
    let mut bye = a.bob.receive("BYE").await;
    assert_eq!(bye.request().to().tag(), Some(LATE_FORK), "and its dialog BYEd");
    bye.respond(200, "OK").await;
}

/// Nothing more reaches the caller; she hangs up on the winner; one CDR,
/// fully reaped; `expected` are the PRACKs this stack sent the callee.
async fn caller_hangs_up(mut a: Answered, expected: &[String]) {
    a.h.advance(PAST_A_ROUND_TRIP).await;
    assert_eq!(a.alice.drain_tu().await, 0, "nothing of the late fork reaches the caller");
    let mut bye = a.dialog.bye().await;
    let mut bob_bye = a.bob.receive("BYE").await;
    assert_eq!(bob_bye.request().to().tag(), Some(WINNER), "the call kept the winner");
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| a.b2bua.is_reaped()).await;
    a.b2bua.assert_fully_reaped();
    assert_eq!(a.b2bua.cdr_records().len(), 1, "one CDR");
    let report = a.h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), a.b2bua.addr, a.bob.addr()),
        expected,
        "one PRACK per RSeq toward the callee"
    );
}

/// The PRACK lines [`pracks_to`] reads for the early fork (when
/// `rang`) and the late fork, on INVITE CSeq `cseq`.
fn pracked(cseq: u32, rang: bool) -> Vec<String> {
    let early = format!("{EARLY_FORK}: {EARLY_FORK_RSEQ} {cseq} INVITE");
    let late = format!("{LATE_FORK}: {LATE_FORK_RSEQ} {cseq} INVITE");
    if rang {
        vec![early, late]
    } else {
        vec![late]
    }
}

/// Transparent relay: the late fork's 183 is PRACKed here, never shown to the
/// caller; the late fork then answers 200 and is ACKed and BYEd.
#[tokio::test(start_paused = true)]
async fn a_late_forks_reliable_183_is_pracked_here_then_its_2xx_released() {
    let name = "b2bua-prack-late-sibling-2xx";
    let mut r = rung_by_an_early_fork(name, Relay::Transparent, &[HELD]).await;
    late_fork_rings(&mut r, ANSWER).await;
    let mut a = winner_answers(r, ANSWER, None).await;
    expect_the_late_forks_prack(&a).await.respond(200, "OK").await;
    late_fork_answers(&mut a, ANSWER).await;
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// Transparent relay: the late fork's 183 is PRACKed here; the fork never
/// sends a final this stack sees (its forking proxy CANCELled it and absorbs
/// the 487), so its early dialog ends at the 64·T1 bound with nothing on the
/// wire, and the call stays up on the winner.
#[tokio::test(start_paused = true)]
async fn a_late_forks_reliable_183_is_pracked_here_then_its_early_dialog_lapses() {
    let name = "b2bua-prack-late-sibling-bound";
    let mut r = rung_by_an_early_fork(name, Relay::Transparent, &[HELD]).await;
    late_fork_rings(&mut r, ANSWER).await;
    let a = winner_answers(r, ANSWER, None).await;
    expect_the_late_forks_prack(&a).await.respond(200, "OK").await;
    a.h.advance(PAST_THE_EARLY_DIALOG_BOUND).await;
    assert_eq!(a.bob.drain_tu().await, 0, "the early dialog lapses with nothing sent");
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// The fabric holds the late fork's 183 past 64·T1 after the winner's 2xx:
/// this stack, its UAC, holds that early dialog terminated by then (RFC 3261
/// §13.2.2.4), so the 183 draws no PRACK and the call stays up on the winner.
#[tokio::test(start_paused = true)]
async fn a_late_forks_183_past_the_early_dialog_bound_draws_no_prack() {
    let name = "b2bua-prack-late-sibling-past-bound";
    let holds = &[PAST_THE_EARLY_DIALOG_BOUND];
    let mut r = rung_by_an_early_fork(name, Relay::Transparent, holds).await;
    late_fork_rings(&mut r, ANSWER).await;
    let a = winner_answers(r, ANSWER, None).await;
    a.h.advance(PAST_THE_EARLY_DIALOG_BOUND).await;
    assert_eq!(a.bob.drain_tu().await, 0, "no PRACK past the bound");
    let early_only = vec![format!("{EARLY_FORK}: {EARLY_FORK_RSEQ} {} INVITE", a.invite_cseq)];
    caller_hangs_up(a, &early_only).await;
}

/// Fake-prack: this stack PRACKed the early fork itself; the late fork's 183
/// draws a PRACK the same way after the answer, and its 2xx is released.
#[tokio::test(start_paused = true)]
async fn under_fake_prack_a_late_forks_reliable_183_is_pracked_then_its_2xx_released() {
    let name = "b2bua-prack-late-sibling-fake-prack";
    let mut r = rung_by_an_early_fork(name, Relay::FakePrack, &[HELD]).await;
    late_fork_rings(&mut r, ANSWER).await;
    let mut a = winner_answers(r, ANSWER, None).await;
    expect_the_late_forks_prack(&a).await.respond(200, "OK").await;
    late_fork_answers(&mut a, ANSWER).await;
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// The late fork retransmits its 183 at T1 and the fabric holds the first
/// copy past the second: the second arrival, while this stack's PRACK awaits
/// its answer, draws no second PRACK (RFC 3262 §4).
#[tokio::test(start_paused = true)]
async fn a_late_forks_retransmitted_183_draws_one_prack() {
    let name = "b2bua-prack-late-sibling-repeat";
    let mut r = rung_by_an_early_fork(name, Relay::Transparent, &[HELD_PAST_T1, HELD]).await;
    late_fork_rings(&mut r, ANSWER).await;
    late_fork_repeats_its_183(&r).await;
    let mut a = winner_answers(r, ANSWER, None).await;
    let mut prack = expect_the_late_forks_prack(&a).await;
    a.h.advance(HELD_PAST_T1 - T1).await;
    assert_eq!(a.bob.drain_tu().await, 0, "no second PRACK for the first copy");
    prack.respond(200, "OK").await;
    late_fork_answers(&mut a, ANSWER).await;
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// An offerless INVITE: the late fork's reliable 183 carries an offer of its
/// own, and the winner answers with its own offer, which the caller answers
/// on her ACK. The PRACK must answer the late fork's offer (RFC 3262 §5):
/// the session is the winner's, so the answer rejects every stream (RFC 3264
/// §6).
#[tokio::test(start_paused = true)]
async fn a_late_forks_offer_is_answered_rejecting_in_the_prack() {
    let mut r = offerless_invite("b2bua-prack-late-sibling-offer").await;
    late_fork_rings(&mut r, LATE_FORK_OFFER).await;
    let mut a = winner_answers(r, WINNER_OFFER, Some(CALLER_ANSWER)).await;
    let mut prack = expect_the_late_forks_prack(&a).await;
    let body = String::from_utf8_lossy(prack.request().body()).into_owned();
    assert!(
        body.lines().any(|l| l.starts_with("m=audio 0 ")),
        "the PRACK answers the late fork's offer, rejecting its stream: {body:?}"
    );
    prack.respond(200, "OK").await;
    late_fork_answers(&mut a, LATE_FORK_OFFER).await;
    let expected = pracked(a.invite_cseq, false);
    caller_hangs_up(a, &expected).await;
}
