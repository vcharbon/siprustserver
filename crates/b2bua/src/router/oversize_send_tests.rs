//! What a worker does when a message it built cannot leave: a datagram past the
//! largest UDP payload is refused by the socket and the refusal is counted. The
//! transaction runs as if the message were lost, so its own timer ends it and the
//! call is released with its CDR (ADR-0001). The cells stand on the bare fabric:
//! the harness's recording fabric fails any send past the payload as a test error.

use std::net::SocketAddr;
use std::time::Duration;

use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::router::test_support::{invite, node_on, src, Node, NODE_SIP_ADDR};

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
/// fit one, and neither does alice's BYE with the line she adds. Each refusal is
/// counted, her BYE is answered, and the leg's Timer F releases the call with its
/// CDR. Bob hears nothing of it, and ends his side on his own 2xx give-up (RFC 3261
/// §13.3.1.4).
#[tokio::test(start_paused = true)]
async fn a_bye_that_cannot_leave_ends_on_its_timer_with_the_cdr() {
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
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(c.send_errors() > 1, "the BYE's refusal is counted");
    assert!(
        next_starting(c.callee.as_ref(), "BYE ", Duration::from_secs(40)).await.is_none(),
        "bob hears nothing of it"
    );
    let answered = next_starting(c.caller.as_ref(), "SIP/2.0 ", Duration::from_millis(10)).await;
    let line = answered.expect("her BYE is answered");
    assert!(line.contains("CSeq: 2 BYE"), "{}", &line[..line.len().min(200)]);
    c.released_with_its_cdr().await;
}

/// A caller's INVITE that fits a datagram, whose b-leg with the worker's Via and
/// Contact does not: nothing reaches the callee, and the worker's first-response
/// timer draws the caller's final with the CDR.
#[tokio::test(start_paused = true)]
async fn a_b_leg_that_cannot_leave_draws_the_callers_final() {
    let c = Cell::new().await;
    let offer = String::from_utf8_lossy(invite("w0", "w1", "oversize-b-leg").image()).to_string();
    let mut pads = String::new();
    let mut left = DATAGRAM - 50 - offer.len();
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
    assert!(grown.len() < DATAGRAM, "{}", grown.len());
    c.caller.send_to(grown.as_bytes(), c.node).await.unwrap();
    let wait = Duration::from_secs(40);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(c.send_errors() > 0, "the b-leg's refusal is counted: {}", grown.len());
    assert!(next_starting(c.callee.as_ref(), "INVITE ", Duration::from_secs(1)).await.is_none());
    let answered = next_starting(c.caller.as_ref(), "SIP/2.0 4", wait).await;
    let answered = answered.or(next_starting(c.caller.as_ref(), "SIP/2.0 5", wait).await);
    answered.expect("the caller draws a final");
    c.released_with_its_cdr().await;
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
