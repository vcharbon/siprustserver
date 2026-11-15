//! **RFC 3262 §3 — a sibling fork's PRACK after the 2xx on another fork is
//! answered 2xx.** A forked callee shows the caller two reliable 183s on two
//! early dialogs; she PRACKs the first, and the callee answers 200 on it. The
//! second fork's 183 is still unacknowledged when that 2xx goes out, so this
//! stack, the UAS on her face, stays prepared to process her PRACK for it:
//! the PRACK draws 200 here and reaches the callee as no second PRACK — the
//! stack PRACKed that fork itself at the 2xx (one PRACK per `RSeq`). A PRACK
//! naming nothing unacknowledged there still draws 481.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, ClientInvite, Dialog, Harness, RunReport, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_message::header::{RAck, RSeq};
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipResponse;
use sip_message::{Method, SipMessage, SipParser, SipRequest};
use sip_net_testkit::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The one-way delay of the crossing scenario.
const TRANSIT_MS: u64 = 20;

/// Each first transmission of a request from `from` to `to`, parsed.
fn requests(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
) -> Vec<(&RecordedSipEntry, SipRequest)> {
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.reemit.is_none())
        .filter_map(|e| match CustomParser::new().parse(&e.raw).expect("parseable SIP") {
            SipMessage::Request(r) => Some((e, r)),
            SipMessage::Response(_) => None,
        })
        .collect()
}

/// The `(To-tag, RAck)` of each PRACK the SUT sent `to`, sorted.
fn pracks_to(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = requests(entries, from, to)
        .into_iter()
        .filter(|(_, r)| *r.method() == Method::Prack)
        .map(|(_, r)| {
            let rack = r.header::<RAck>().expect("an RAck").expect("readable RAck");
            let tag = r.to().tag().unwrap_or_default().to_string();
            (tag, format!("{} {} {}", rack.rseq(), rack.seq(), rack.method()))
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

/// What the forked setup leaves: bob's INVITE server transaction, fork B's
/// 183 as alice saw it, and bob's INVITE CSeq.
struct Forked {
    uas: ServerTxn,
    ring_b: SipResponse,
    invite_cseq: u32,
}

/// Bob forks two reliable 183s and alice PRACKs fork A's only.
async fn two_forks_first_pracked(call: &mut ClientInvite, bob: &Agent) -> Forked {
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
    Forked { uas, ring_b, invite_cseq }
}

/// Bob answers 200 on fork A, then answers the PRACK the stack sends fork B.
async fn answer_fork_a(forked: &mut Forked, bob: &Agent) {
    forked.uas.respond(200, "OK").with_to_tag("forkA").with_sdp(ANSWER).await;
    let mut stacks_own = bob.receive("PRACK").await;
    assert_eq!(stacks_own.request().to().tag(), Some("forkB"), "the stack PRACKs fork B");
    stacks_own.respond(200, "OK").await;
}

/// Alice's next PRACK on fork B naming `rseq` against her INVITE, which
/// draws `status`.
async fn prack_fork_b(call: &mut ClientInvite, ring_b: &SipResponse, rseq: u32, status: u16) {
    let tag = ring_b.to().tag().expect("fork B's tag").to_string();
    let rack = format!("{rseq} {} INVITE", ring_b.cseq().seq());
    call.send_request(InDialogMethod::Prack)
        .with_to_tag(&tag)
        .with_rack(&rack)
        .send()
        .await
        .expect(status)
        .await;
}

/// Hang up and return the run's report, checking bob saw one PRACK per fork.
async fn hang_up_and_count(
    h: Harness,
    b2bua: &B2buaSut,
    mut dialog: Dialog,
    bob: &Agent,
    bob_addr: SocketAddr,
    invite_cseq: u32,
) -> RunReport {
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
    report
}

/// Alice ACKs the 2xx on fork A, then PRACKs fork B's 183: 200 here. Her
/// repeat of that PRACK as a new request, and one naming an `RSeq` never
/// shown, name nothing unacknowledged: 481.
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
    let mut forked = two_forks_first_pracked(&mut call, &bob).await;
    answer_fork_a(&mut forked, &bob).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;

    let cseq = forked.invite_cseq;
    let shown = forked.ring_b.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value();
    prack_fork_b(&mut call, &forked.ring_b, shown, 200).await;
    prack_fork_b(&mut call, &forked.ring_b, shown, 481).await;
    prack_fork_b(&mut call, &forked.ring_b, shown + 1, 481).await;

    hang_up_and_count(h, &b2bua, dialog, &bob, bob_addr, cseq).await;
}

/// Alice's PRACK for fork B crosses the 2xx on fork A: it leaves her after
/// the stack relayed that 2xx and before it reaches her, while the stack's
/// own PRACK to fork B still awaits its answer. 200 here.
#[tokio::test(start_paused = true)]
async fn a_sibling_forks_prack_crossing_the_2xx_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-prack-sibling-fork-crossing", TRANSIT_MS);
    let alice = h.agent("alice", "127.0.0.1:7493").await;
    let bob = h.agent("bob", "127.0.0.1:7494").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7494).start(&h, "b2bua", "127.0.0.1:7495").await;
    let alice_addr: SocketAddr = "127.0.0.1:7493".parse().unwrap();
    let bob_addr: SocketAddr = "127.0.0.1:7494".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut forked = two_forks_first_pracked(&mut call, &bob).await;
    forked.uas.respond(200, "OK").with_to_tag("forkA").with_sdp(ANSWER).await;
    h.advance(Duration::from_millis(TRANSIT_MS * 3 / 2)).await;
    let (mut crossing, _) =
        call.try_prack_with_request(&forked.ring_b).await.expect("alice PRACKs fork B's 183");
    let mut stacks_own = bob.receive("PRACK").await;
    assert_eq!(stacks_own.request().to().tag(), Some("forkB"), "the stack PRACKs fork B");
    crossing.expect(200).await;
    stacks_own.respond(200, "OK").await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;

    let cseq = forked.invite_cseq;
    let report = hang_up_and_count(h, &b2bua, dialog, &bob, bob_addr, cseq).await;
    let entries = report.entries();
    let (prack, _) = requests(&entries, alice_addr, b2bua.addr)
        .into_iter()
        .find(|(_, r)| *r.method() == Method::Prack && r.to().tag() == forked.ring_b.to().tag())
        .expect("alice's fork-B PRACK on the wire");
    let answer = entries
        .iter()
        .find(|e| e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 200 ")
            && matches!(CustomParser::new().parse(&e.raw), Ok(SipMessage::Response(r)) if *r.cseq().method() == Method::Invite))
        .expect("the 2xx relayed to alice");
    assert!(
        prack.sent_ms < answer.received_ms.expect("delivered"),
        "her PRACK left before the 2xx reached her"
    );
    assert!(
        prack.received_ms.expect("delivered") > answer.sent_ms,
        "and reached the stack after it left"
    );
}
