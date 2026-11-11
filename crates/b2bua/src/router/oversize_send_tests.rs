//! What a worker does when a message it built cannot leave (ADR-0001). A request
//! that would open a client transaction past the largest UDP payload fails at
//! once, as a dead hop's does: a b-leg INVITE reaches the failure consult, an
//! in-dialog request its timeout rule. Any other datagram past the payload is
//! refused by the socket and the refusal is counted; its transaction runs as if
//! it were lost. The cells stand on the bare fabric: the harness's recording
//! fabric fails any send past the payload as a test error.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::decision::test_adapter::route_to;
use crate::decision::{CallFailureRequest, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use crate::router::test_support::{invite, node_deciding, node_on, src, Node, NODE_SIP_ADDR};

/// The callee the scripted decision routes every call to.
const CALLEE: ([u8; 4], u16) = ([127, 0, 0, 2], 9);
/// The largest UDP payload over IPv4.
const DATAGRAM: usize = 65_507;
const SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.2\r\ns=-\r\nc=IN IP4 127.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

struct Cell {
    n: Node,
    caller: Box<dyn UdpEndpoint>,
    callee: Box<dyn UdpEndpoint>,
    node: SocketAddr,
}

impl Cell {
    async fn new() -> Cell {
        let net = SimulatedSignalingNetwork::new(1);
        let n = node_on(&net, "w0", |_| {}, Vec::new()).await;
        let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
        let callee = net.bind_udp(BindUdpOpts::new(SocketAddr::from(CALLEE), 64)).await.unwrap();
        Cell { n, caller, callee, node: SocketAddr::from(NODE_SIP_ADDR) }
    }

    fn send_errors(&self) -> u64 {
        self.n.core.txn_metrics().send_errors()
    }

    /// The call is released with its one CDR.
    async fn released_with_its_cdr(&self) {
        sip_clock::testkit::settle().await;
        assert_eq!(self.n.core.router_ctx().state.active_count(), 0, "the call is released");
        assert_eq!(self.n.cdr.snapshot().len(), 1, "with its CDR");
    }
}

/// The next datagram `ep` receives within `wait` whose first line starts with
/// `start`, as text.
async fn next_starting(ep: &dyn UdpEndpoint, start: &str, wait: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let packet = tokio::time::timeout_at(deadline, ep.recv()).await.ok()??;
        let text = String::from_utf8_lossy(&packet.raw).to_string();
        if text.starts_with(start) {
            return Some(text);
        }
    }
}

/// The value of `text`'s first header line `name`.
fn header<'a>(text: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name}:");
    let line = text.lines().find(|l| l.starts_with(&prefix)).expect("the header is present");
    line[prefix.len()..].trim()
}

/// The 200 `tag` answers `invite` with, filled to its datagram by a Contact URI,
/// or by `X-Fill` lines when `contact` is false.
fn answer_filling(invite: &str, tag: &str, contact: bool) -> String {
    let vias: String =
        invite.lines().filter(|l| l.starts_with("Via:")).map(|l| format!("{l}\r\n")).collect();
    let fixed = format!(
        "SIP/2.0 200 OK\r\n{vias}From: {}\r\nTo: {};tag={tag}\r\nCall-ID: {}\r\nCSeq: {}\r\n",
        header(invite, "From"),
        header(invite, "To"),
        header(invite, "Call-ID"),
        header(invite, "CSeq"),
    );
    let tail =
        format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP}", SDP.len());
    let frame = "Contact: <sip:bob@127.0.0.2:9;x=>\r\n";
    let left = DATAGRAM - fixed.len() - tail.len() - frame.len();
    if contact {
        format!("{fixed}Contact: <sip:bob@127.0.0.2:9;x={}>\r\n{tail}", "p".repeat(left))
    } else {
        let contact = "Contact: <sip:bob@127.0.0.2:9>\r\n";
        let fill = left + frame.len() - contact.len() - "X-Fill: \r\n".len();
        format!("{fixed}{contact}X-Fill: {}\r\n{tail}", "f".repeat(fill))
    }
}

/// Bob's 2xx names a Contact that fills a datagram: the ACK to that target does not
/// fit one, and neither does alice's BYE with the line she adds. The ACK's refusal
/// by the socket is counted. The BYE fails at once as a timed-out BYE does: her
/// BYE is answered and the call is released with its CDR, with no timer elapsed.
/// Bob hears nothing of it, and ends his side on his own 2xx give-up (RFC 3261
/// §13.3.1.4).
#[tokio::test(start_paused = true)]
async fn a_bye_that_cannot_leave_releases_the_call_at_once_with_the_cdr() {
    let c = Cell::new().await;
    let offer = invite("w0", "w1", "oversize-bye");
    c.caller.send_to(offer.image(), c.node).await.unwrap();
    let wait = Duration::from_secs(1);
    let dialled = next_starting(c.callee.as_ref(), "INVITE ", wait).await.expect("dialled");
    let answer = answer_filling(&dialled, "bobtag", true);
    assert_eq!(answer.len(), DATAGRAM, "bob's 2xx fits exactly");
    c.callee.send_to(answer.as_bytes(), c.node).await.unwrap();
    let ok = next_starting(c.caller.as_ref(), "SIP/2.0 200", wait).await.expect("relayed");
    assert_eq!(c.send_errors(), 0, "nothing was refused yet");

    let target = header(&ok, "Contact").trim_start_matches('<').trim_end_matches('>').to_string();
    let to = header(&ok, "To").to_string();
    let from = header(&ok, "From").to_string();
    let call_id = header(&ok, "Call-ID").to_string();
    let ack = format!(
        "ACK {target} SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-ack\r\nMax-Forwards: 70\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: 1 ACK\r\nContent-Length: 0\r\n\r\n"
    );
    c.caller.send_to(ack.as_bytes(), c.node).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(c.send_errors(), 1, "the ACK's refusal is counted");
    assert!(next_starting(c.callee.as_ref(), "ACK ", wait).await.is_none(), "bob has no ACK");

    let bye = format!(
        "BYE {target} SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-bye\r\nMax-Forwards: 70\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: 2 BYE\r\nX-Grow: {}\r\nContent-Length: 0\r\n\r\n",
        "g".repeat(800)
    );
    c.caller.send_to(bye.as_bytes(), c.node).await.unwrap();
    let answered = next_starting(c.caller.as_ref(), "SIP/2.0 ", Duration::from_millis(100)).await;
    let line = answered.expect("her BYE is answered");
    assert!(line.contains("CSeq: 2 BYE"), "{}", &line[..line.len().min(200)]);
    c.released_with_its_cdr().await;
    assert_eq!(c.send_errors(), 1, "the BYE never reached the socket");
    assert_eq!(c.n.metrics.outbound_unsent_of("too_long", "request"), 1, "the BYE is counted");
    assert!(
        next_starting(c.callee.as_ref(), "BYE ", Duration::from_secs(40)).await.is_none(),
        "bob hears nothing of it"
    );
}

/// The callee the failure consult routes the call to.
const SECOND: ([u8; 4], u16) = ([127, 0, 0, 3], 9);

/// The 200 `tag` answers `invite` with, carrying an SDP answer.
fn answer(invite: &str, tag: &str) -> String {
    let vias: String =
        invite.lines().filter(|l| l.starts_with("Via:")).map(|l| format!("{l}\r\n")).collect();
    format!(
        "SIP/2.0 200 OK\r\n{vias}From: {}\r\nTo: {};tag={tag}\r\nCall-ID: {}\r\nCSeq: {}\r\nContact: <sip:carol@127.0.0.3:9>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP}",
        header(invite, "From"),
        header(invite, "To"),
        header(invite, "Call-ID"),
        header(invite, "CSeq"),
        SDP.len(),
    )
}

/// A caller's INVITE that fits a datagram, whose b-leg with the first route's
/// Request-URI and the worker's Via and Contact does not: nothing reaches that
/// callee, and the attempt fails at once into the failure consult, as a dead
/// hop's does, with no timer elapsed. The consult's route fits and connects;
/// the call ends on the caller's BYE and is released with its CDR.
#[tokio::test(start_paused = true)]
async fn a_b_leg_that_cannot_leave_draws_the_callers_final() {
    let consulted = Arc::new(Mutex::new(Vec::new()));
    let seen = consulted.clone();
    let decision = ScriptedDecisionEngine::builder()
        .fallback(|_| {
            let mut first = route_to("127.0.0.2", 9);
            first.new_ruri = Some(format!("sip:{}@127.0.0.2:9", "u".repeat(6_000)));
            first.callback_context = Some("first".into());
            NewCallResponse::Route(first)
        })
        .on_failure(move |req: &CallFailureRequest| {
            let at = tokio::time::Instant::now();
            seen.lock().unwrap().push((req.failure.clone(), at));
            CallTreatment::Route(route_to("127.0.0.3", 9))
        })
        .build();
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_deciding(&net, "w0", Arc::new(decision), |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let callee = net.bind_udp(BindUdpOpts::new(SocketAddr::from(CALLEE), 64)).await.unwrap();
    let second = net.bind_udp(BindUdpOpts::new(SocketAddr::from(SECOND), 64)).await.unwrap();
    let node = SocketAddr::from(NODE_SIP_ADDR);

    let offer = String::from_utf8_lossy(invite("w0", "w1", "oversize-b-leg").image()).to_string();
    let mut pads = String::new();
    let mut left = DATAGRAM - 3_000 - offer.len();
    for n in 0.. {
        let frame = format!("X-Fill-{n}: \r\n").len();
        if left <= frame {
            break;
        }
        let each = (left - frame).min(8_000);
        pads.push_str(&format!("X-Fill-{n}: {}\r\n", "f".repeat(each)));
        left -= frame + each;
    }
    let grown = offer.replacen("Content-Length:", &format!("{pads}Content-Length:"), 1);
    let sent_at = tokio::time::Instant::now();
    caller.send_to(grown.as_bytes(), node).await.unwrap();
    let wait = Duration::from_millis(50);
    let dialled = next_starting(second.as_ref(), "INVITE ", wait).await;
    let dialled = dialled.expect("the second route is dialled at once, with no timer elapsed");
    {
        let consults = consulted.lock().unwrap();
        let [(failure, at)] = consults.as_slice() else { panic!("one consult: {consults:?}") };
        assert_eq!(failure.origin, "transaction_timeout");
        assert_eq!(failure.timeout_kind.as_deref(), Some("response"), "as for a dead hop");
        assert!(at.duration_since(sent_at) < wait, "no timer elapsed before the consult");
    }
    assert!(next_starting(callee.as_ref(), "INVITE ", wait).await.is_none(), "nothing left");
    assert_eq!(n.core.txn_metrics().send_errors(), 0, "nothing reached the socket");
    assert_eq!(n.metrics.outbound_unsent_of("too_long", "request"), 1, "the INVITE is counted");

    second.send_to(answer(&dialled, "carol").as_bytes(), node).await.unwrap();
    let ok = next_starting(caller.as_ref(), "SIP/2.0 200", wait).await.expect("connected");
    let target = header(&ok, "Contact").trim_start_matches('<').trim_end_matches('>').to_string();
    let (to, from, call_id) = (header(&ok, "To"), header(&ok, "From"), header(&ok, "Call-ID"));
    let in_dialog = |method: &str, seq: u32| {
        format!(
            "{method} {target} SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{seq}\r\nMax-Forwards: 70\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {seq} {method}\r\nContent-Length: 0\r\n\r\n"
        )
    };
    caller.send_to(in_dialog("ACK", 1).as_bytes(), node).await.unwrap();
    next_starting(second.as_ref(), "ACK ", wait).await.expect("the ACK reaches the callee");
    caller.send_to(in_dialog("BYE", 2).as_bytes(), node).await.unwrap();
    let bye = next_starting(second.as_ref(), "BYE ", wait).await.expect("the BYE is relayed");
    let vias: String =
        bye.lines().filter(|l| l.starts_with("Via:")).map(|l| format!("{l}\r\n")).collect();
    let bye_ok = format!(
        "SIP/2.0 200 OK\r\n{vias}From: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
        header(&bye, "From"),
        header(&bye, "To"),
        header(&bye, "Call-ID"),
        header(&bye, "CSeq"),
    );
    second.send_to(bye_ok.as_bytes(), node).await.unwrap();
    let answered = next_starting(caller.as_ref(), "SIP/2.0 200", wait).await;
    assert!(answered.expect("her BYE is answered").contains("CSeq: 2 BYE"));
    tokio::time::sleep(wait).await;
    assert_eq!(n.core.router_ctx().state.active_count(), 0, "the call is released");
    assert_eq!(n.cdr.snapshot().len(), 1, "with its CDR");
}

/// Bob's 2xx fills a datagram with a line the worker relays, and the worker's own
/// lines on the caller's face push it past one: the 2xx cannot leave, its ladder
/// runs to its give-up, and the call is released with its CDR. The caller, who
/// heard nothing past her provisional, gives up on her own Timer B.
#[tokio::test(start_paused = true)]
async fn a_2xx_that_cannot_reach_the_caller_ends_on_its_ladder_with_the_cdr() {
    let c = Cell::new().await;
    let offer = invite("w0", "w1", "oversize-2xx");
    c.caller.send_to(offer.image(), c.node).await.unwrap();
    let wait = Duration::from_secs(1);
    let dialled = next_starting(c.callee.as_ref(), "INVITE ", wait).await.expect("dialled");
    let answer = answer_filling(&dialled, "bobtag", false);
    c.callee.send_to(answer.as_bytes(), c.node).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(c.send_errors() > 0, "the 2xx's refusal is counted");
    assert!(next_starting(c.caller.as_ref(), "SIP/2.0 200", wait).await.is_none(), "not heard");
    // The ladder's give-up BYEs bob, who answers.
    let bye = next_starting(c.callee.as_ref(), "BYE ", Duration::from_secs(40)).await;
    let bye = bye.expect("the give-up ends bob's leg");
    let vias: String =
        bye.lines().filter(|l| l.starts_with("Via:")).map(|l| format!("{l}\r\n")).collect();
    let ok = format!(
        "SIP/2.0 200 OK\r\n{vias}From: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
        header(&bye, "From"),
        header(&bye, "To"),
        header(&bye, "Call-ID"),
        header(&bye, "CSeq"),
    );
    c.callee.send_to(ok.as_bytes(), c.node).await.unwrap();
    tokio::time::sleep(Duration::from_secs(40)).await;
    c.released_with_its_cdr().await;
}
