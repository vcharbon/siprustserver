//! **RFC 3262 §4 — a sibling fork's first reliable provisional reaching this
//! stack after another fork's 2xx is PRACKed here.** A forked callee's 2xx on
//! one fork established the call; another fork's reliable 183 reaches this
//! stack only afterwards. Its early dialog lives on (RFC 3261 §13.2.2.4), and
//! this stack, that fork's UAC, is the only party that took the provisional:
//! it PRACKs it once, on that fork's own early dialog, and relays nothing
//! toward the caller, whose INVITE transaction already sent its final
//! (RFC 3261 §13.3.1.1). The early dialog then ends by the fork's own 2xx
//! (ACKed and BYEd as any straggler) or silently at the 64·T1 bound; the call
//! keeps the winner. Under the transparent relay and under fake-prack alike.
//!
//! The harness's fabric has one transit delay per hop and cannot reorder, so
//! the callee here sends the late 183 after his 2xx, as a datagram of its own:
//! on the wire this stack sees, that is the 183 sent before the 2xx and
//! overtaken by it in flight. Each test waives the two after-final rules on
//! the callee for that one message.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, stated, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Agent, Dialog, Harness, ServerTxn, WaiverScope};
use sip_net_testkit::RecordedSipEntry;

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

/// Past a round trip on the harness's 1 ms transit, well inside T1.
const PAST_A_ROUND_TRIP: Duration = Duration::from_millis(50);

/// Past 64·T1 (32 s) after the late fork's 183: the bound of its early dialog.
const PAST_THE_EARLY_DIALOG_BOUND: Duration = Duration::from_secs(33);

/// Who relays the callee's provisionals before the answer.
#[derive(Clone, Copy)]
enum Relay {
    /// The caller sees each reliable 183 and PRACKs it.
    Transparent,
    /// The caller sees a bare 180; this stack PRACKs the callee itself.
    FakePrack,
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

/// The waivers of the callee's late 183: the crossing the fabric cannot stage.
fn late_183_crossed_the_2xx(h: &Harness) {
    for rule in ["no-1xx-after-final", "no-new-reliable-1xx-after-final"] {
        h.waive(
            WaiverScope::rule(
                rule,
                "the late fork's 183 left its UAS before the winner's 2xx and was overtaken by it \
                 in flight; the fabric cannot reorder, so the callee sends it after",
            )
            .on_party("bob"),
        );
    }
}

/// A harness, its parties and this stack under `relay`, with the keepalive
/// out of every window these tests wait through.
async fn parties(name: &str, relay: Relay) -> (Harness, Agent, Agent, B2buaSut) {
    let h = Harness::with_transit_delay(name, 1);
    late_183_crossed_the_2xx(&h);
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
/// the caller or by this stack as `relay` has it, then [`WINNER`] answers
/// 200 and the caller ACKs.
async fn answered_after_an_early_fork(name: &str, relay: Relay) -> Answered {
    let (h, alice, bob, b2bua) = parties(name, relay).await;
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
        .with_header("Require", "100rel")
        .with_header("RSeq", &EARLY_FORK_RSEQ.to_string())
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
    uas.respond(200, "OK").with_to_tag(WINNER).with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    Answered { h, alice, bob, b2bua, uas, dialog, invite_cseq }
}

/// The callee does not ring: [`WINNER`] answers an offerless INVITE 200 with
/// its own offer and the caller answers it on her ACK.
async fn answered_with_the_winners_offer(name: &str) -> Answered {
    let (h, alice, bob, b2bua) = parties(name, Relay::Transparent).await;
    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(uas.request().sdp().is_none(), "an offerless INVITE reaches the callee");
    let invite_cseq = uas.request().cseq().seq();
    uas.respond(200, "OK").with_to_tag(WINNER).with_sdp(WINNER_OFFER).await;
    call.expect(200).await;
    let dialog = call.ack_with(Some(CALLER_ANSWER)).await;
    bob.receive("ACK").await;
    Answered { h, alice, bob, b2bua, uas, dialog, invite_cseq }
}

/// [`LATE_FORK`]'s first reliable 183, with `RSeq` [`LATE_FORK_RSEQ`] and
/// `sdp`: the winner's 2xx as the callee sent it, retagged and turned into a
/// provisional under the same Via and CSeq.
fn late_reliable_183(h: &Harness, bob: SocketAddr, b2bua: SocketAddr, sdp: &str) -> Vec<u8> {
    let answer = h
        .wire_entries()
        .into_iter()
        .filter(|e| e.from == bob && e.to == b2bua && e.raw.starts_with(b"SIP/2.0 200 "))
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .find(|text| {
            text.split("\r\n").any(|l| l.starts_with("CSeq:") && l.trim_end().ends_with("INVITE"))
        })
        .expect("the winner's 2xx");
    let (head, _) = answer.split_once("\r\n\r\n").expect("the 2xx's head");
    let winner_tag = format!(";tag={WINNER}");
    let late_tag = format!(";tag={LATE_FORK}");
    let head: Vec<String> = head
        .split("\r\n")
        .skip(1)
        .filter(|l| !l.to_ascii_lowercase().starts_with("content-"))
        .map(|l| l.replace(&winner_tag, &late_tag))
        .collect();
    format!(
        "SIP/2.0 183 Session Progress\r\n{}\r\nRequire: 100rel\r\nRSeq: {LATE_FORK_RSEQ}\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        head.join("\r\n"),
        sdp.len()
    )
    .into_bytes()
}

/// The callee puts [`LATE_FORK`]'s 183 carrying `sdp` on the wire.
async fn send_late_183(a: &Answered, sdp: &str) -> Vec<u8> {
    let late = late_reliable_183(&a.h, a.bob.addr(), a.b2bua.addr, sdp);
    a.bob.try_send_datagram(&late, a.b2bua.addr).await.expect("the late 183 leaves");
    late
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

/// The `(To-tag, RAck)` of each PRACK this stack first sent `to`, in order.
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

/// The PRACK lines [`pracks_to`] reads for the early fork (when `rang`) and
/// the late fork, on INVITE CSeq `cseq`.
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
    let mut a =
        answered_after_an_early_fork("b2bua-prack-late-sibling-2xx", Relay::Transparent).await;
    send_late_183(&a, ANSWER).await;
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
    let a =
        answered_after_an_early_fork("b2bua-prack-late-sibling-bound", Relay::Transparent).await;
    send_late_183(&a, ANSWER).await;
    expect_the_late_forks_prack(&a).await.respond(200, "OK").await;
    a.h.advance(PAST_THE_EARLY_DIALOG_BOUND).await;
    assert_eq!(a.bob.drain_tu().await, 0, "the early dialog lapses with nothing sent");
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// Fake-prack: this stack PRACKed the early fork itself; the late fork's 183
/// draws a PRACK the same way after the answer, and its 2xx is released.
#[tokio::test(start_paused = true)]
async fn under_fake_prack_a_late_forks_reliable_183_is_pracked_then_its_2xx_released() {
    let mut a =
        answered_after_an_early_fork("b2bua-prack-late-sibling-fake-prack", Relay::FakePrack).await;
    send_late_183(&a, ANSWER).await;
    expect_the_late_forks_prack(&a).await.respond(200, "OK").await;
    late_fork_answers(&mut a, ANSWER).await;
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// The late fork retransmits its 183 while this stack's PRACK awaits its
/// answer: that copy draws no second PRACK (RFC 3262 §4); the PRACK
/// transaction re-sends itself.
#[tokio::test(start_paused = true)]
async fn a_late_forks_retransmitted_183_draws_one_prack() {
    let mut a =
        answered_after_an_early_fork("b2bua-prack-late-sibling-repeat", Relay::Transparent).await;
    let late = send_late_183(&a, ANSWER).await;
    let mut prack = expect_the_late_forks_prack(&a).await;
    a.bob.try_send_datagram(&late, a.b2bua.addr).await.expect("the copy leaves");
    a.h.advance(PAST_A_ROUND_TRIP).await;
    assert_eq!(a.bob.drain_tu().await, 0, "no second PRACK for the copy");
    prack.respond(200, "OK").await;
    late_fork_answers(&mut a, ANSWER).await;
    let expected = pracked(a.invite_cseq, true);
    caller_hangs_up(a, &expected).await;
}

/// An offerless INVITE the winner answered with its own offer; the late
/// fork's reliable 183 carries an offer of its own. The PRACK must answer it
/// (RFC 3262 §5): the session is the winner's, so the answer rejects every
/// stream (RFC 3264 §6).
#[tokio::test(start_paused = true)]
async fn a_late_forks_offer_is_answered_rejecting_in_the_prack() {
    let mut a = answered_with_the_winners_offer("b2bua-prack-late-sibling-offer").await;
    send_late_183(&a, LATE_FORK_OFFER).await;
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
