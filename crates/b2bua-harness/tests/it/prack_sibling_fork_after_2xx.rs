//! **RFC 3262 §3 — a sibling fork's PRACK after the 2xx on another fork is
//! answered 2xx.** A forked callee shows the caller two reliable 183s on two
//! early dialogs; she PRACKs the first, and the callee answers 200 on it. The
//! second fork's 183 is still unacknowledged when that 2xx goes out, so this
//! stack, the UAS on her face, stays prepared to process her PRACK for it:
//! the PRACK draws 200 here and reaches the callee as no second PRACK — the
//! stack PRACKed that fork itself at the 2xx (one PRACK per `RSeq`).

use std::net::SocketAddr;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{ClientInvite, Harness, ServerTxn};
use sip_message::types::SipResponse;
use sip_net_testkit::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The `(To-tag, RAck)` of each PRACK the SUT put on `to`'s wire, sorted.
fn pracks_to(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"PRACK "))
        .filter(|e| e.reemit.is_none())
        .filter_map(|e| {
            let text = std::str::from_utf8(&e.raw).ok()?;
            let line = |name: &str| {
                text.split("\r\n")
                    .find(|l| l.len() > name.len() && l[..name.len()].eq_ignore_ascii_case(name))
                    .map(|l| l[name.len()..].trim().to_string())
            };
            let tag = line("To:")?.split(";tag=").nth(1)?.split(';').next()?.to_string();
            Some((tag, line("RAck:")?))
        })
        .collect();
    out.sort();
    out
}

/// The callee's reliable 183 with its answer, on fork `tag` with `rseq`.
fn reliable_183<'a>(
    uas: &'a mut ServerTxn,
    tag: &str,
    rseq: &str,
) -> scenario_harness::Respond<'a> {
    uas.respond(183, "Session Progress")
        .with_to_tag(tag)
        .with_header("Require", "100rel")
        .with_header("RSeq", rseq)
        .with_sdp(ANSWER)
}

/// Bob forks two reliable 183s; alice PRACKs fork A's only; bob answers 200
/// on fork A. Returns fork B's 183 as alice saw it, and bob's INVITE CSeq.
async fn two_forks_answered_on_the_first(
    call: &mut ClientInvite,
    bob: &scenario_harness::Agent,
) -> (SipResponse, u32) {
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();
    reliable_183(&mut uas, "forkA", "1").await;
    let ring_a = call.expect(183).await;
    reliable_183(&mut uas, "forkB", "1").await;
    let ring_b = call.expect(183).await;

    let (mut prack_a, _) =
        call.try_prack_with_request(&ring_a).await.expect("alice PRACKs fork A's 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack_a.expect(200).await;

    uas.respond(200, "OK").with_to_tag("forkA").with_sdp(ANSWER).await;
    let mut stacks_own = bob.receive("PRACK").await;
    assert_eq!(
        stacks_own.request().to().tag(),
        Some("forkB"),
        "the stack PRACKs fork B at the 2xx"
    );
    stacks_own.respond(200, "OK").await;
    (ring_b, invite_cseq)
}

/// Hang up the confirmed call and check bob saw one PRACK per fork.
async fn hang_up_and_count(
    h: Harness,
    b2bua: B2buaSut,
    mut dialog: scenario_harness::Dialog,
    bob: &scenario_harness::Agent,
    bob_addr: SocketAddr,
    invite_cseq: u32,
) {
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), b2bua.addr, bob_addr),
        vec![
            ("forkA".to_string(), format!("1 {invite_cseq} INVITE")),
            ("forkB".to_string(), format!("1 {invite_cseq} INVITE")),
        ],
        "one PRACK per fork toward the callee"
    );
}

/// Alice ACKs the 2xx on fork A, then PRACKs fork B's 183: 200 here.
#[tokio::test(start_paused = true)]
async fn a_sibling_forks_prack_after_the_ack_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-prack-sibling-fork-after-ack", 0);
    let alice = h.agent("alice", "127.0.0.1:7490").await;
    let bob = h.agent("bob", "127.0.0.1:7491").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7491).start(&h, "b2bua", "127.0.0.1:7492").await;
    let bob_addr: SocketAddr = "127.0.0.1:7491".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let (ring_b, invite_cseq) = two_forks_answered_on_the_first(&mut call, &bob).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;

    let (mut late, _) =
        call.try_prack_with_request(&ring_b).await.expect("alice PRACKs fork B's 183");
    late.expect(200).await;

    hang_up_and_count(h, b2bua, dialog, &bob, bob_addr, invite_cseq).await;
}

/// Alice's PRACK for fork B crosses the 2xx on fork A: she sends it before
/// her ACK, and it reaches this stack after the 2xx went out. 200 here.
#[tokio::test(start_paused = true)]
async fn a_sibling_forks_prack_crossing_the_2xx_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-prack-sibling-fork-crossing", 0);
    let alice = h.agent("alice", "127.0.0.1:7493").await;
    let bob = h.agent("bob", "127.0.0.1:7494").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7494).start(&h, "b2bua", "127.0.0.1:7495").await;
    let bob_addr: SocketAddr = "127.0.0.1:7494".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let (ring_b, invite_cseq) = two_forks_answered_on_the_first(&mut call, &bob).await;
    call.expect(200).await;
    let (mut late, _) =
        call.try_prack_with_request(&ring_b).await.expect("alice PRACKs fork B's 183");
    late.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;

    hang_up_and_count(h, b2bua, dialog, &bob, bob_addr, invite_cseq).await;
}
