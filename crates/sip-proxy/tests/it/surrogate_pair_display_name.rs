//! A caller's display name holding one character as a surrogate pair encoded
//! byte by byte (CESU-8: `ED A0 BC ED BC B9` for U+1F339) reads as that
//! character at the proxy: a whole call crosses it, nothing it sends holds a
//! surrogate half, and every From it relays holds the character in UTF-8
//! (`F0 9F 8C B9`) with the URI and tag as the caller wrote them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::{Contact, Uri};
use sip_message::{CustomParser, SipMessage, SipParser, SipRequest};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, RoutingStrategy};
use sip_txn::IdGen;

const PROXY: &str = "192.0.2.1:5060";
const CALLER: &str = "192.0.2.20:5060";
const WORKER: &str = "10.0.0.2:5070";

const CESU8_ROSE: &[u8] = b"\xed\xa0\xbc\xed\xbc\xb9";
const UTF8_ROSE: &[u8] = b"\xf0\x9f\x8c\xb9";
const CALLEE_CONTACT: &str = "sip:bob@10.0.0.2:5070";
const FROM_TAIL: &str = "<sip:alice@192.0.2.20>;tag=a-rose";

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// The caller's request of `method`, its display name in CESU-8; `to_tag` and
/// `route` for a request inside the dialog, whose Request-URI is the callee's
/// Contact.
fn request(
    method: &str,
    cseq: u32,
    branch: &str,
    to_tag: Option<&str>,
    route: &[String],
) -> Vec<u8> {
    let ruri = if to_tag.is_some() { CALLEE_CONTACT } else { "sip:bob@192.0.2.30" };
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let route: String = route.iter().map(|r| format!("Route: {r}\r\n")).collect();
    let head = format!(
        "{method} {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-{branch};rport\r\n\
         {route}\
         Max-Forwards: 70\r\n\
         From: \"Rose "
    );
    let tail = format!(
        "\" {FROM_TAIL}\r\n\
         To: <sip:bob@192.0.2.30>{to_tag}\r\n\
         Call-ID: rose@192.0.2.20\r\n\
         CSeq: {cseq} {method}\r\n\
         Contact: <sip:alice@{CALLER}>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    [head.as_bytes(), CESU8_ROSE, tail.as_bytes()].concat()
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Whether `bytes` holds a surrogate half's lead: `ED` then `A0`..`BF`.
fn holds_a_surrogate_half(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|w| w[0] == 0xed && (0xa0..=0xbf).contains(&w[1]))
}

/// The From line of `raw`.
fn from_line(raw: &[u8]) -> Vec<u8> {
    raw.split(|&b| b == b'\n')
        .find(|line| line.starts_with(b"From:"))
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

fn parsed(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).unwrap_or_else(|e| panic!("reads: {}", e.reason))
}

fn read_request(raw: &[u8]) -> SipRequest {
    match parsed(raw) {
        SipMessage::Request(req) => req,
        other => panic!("a request: {other:?}"),
    }
}

/// What the proxy sent, checked: no surrogate half, the From holding the
/// character in UTF-8 and the URI and tag as written.
#[track_caller]
fn assert_relayed(raw: &[u8], what: &str) {
    assert!(!holds_a_surrogate_half(raw), "{what}: no surrogate half leaves the proxy");
    let from = from_line(raw);
    assert!(holds(&from, UTF8_ROSE), "{what}: the From holds the character in UTF-8: {from:?}");
    assert!(holds(&from, FROM_TAIL.as_bytes()), "{what}: URI and tag as written: {from:?}");
}

#[tokio::test(start_paused = true)]
async fn a_surrogate_pair_in_the_callers_display_name_crosses_the_proxy_as_its_character() {
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), 64));
    let endpoint = bind(PROXY).await.expect("bind the proxy");
    let caller: Box<dyn UdpEndpoint> = bind(CALLER).await.expect("bind the caller");
    let worker: Box<dyn UdpEndpoint> = bind(WORKER).await.expect("bind the worker");
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
    let registry: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w0",
            ProxyAddr::from(addr(WORKER)),
        )]));
    let core = ProxyCoreBuilder::new(
        ProxyAddr::from(addr(PROXY)),
        strategy,
        registry,
        Arc::new(IdGen::seeded(0x5ead)),
    )
    .clock(Clock::test_at(0))
    .build(endpoint);
    let task = tokio::spawn(core.run());

    caller.send_to(&request("INVITE", 1, "inv", None, &[]), addr(PROXY)).await.expect("sent");
    settle().await;
    let invite = worker.try_recv().expect("the INVITE reaches the worker").raw.to_vec();
    assert_relayed(&invite, "the INVITE");

    let opts = GenerateResponseOpts {
        to_tag: Some("b-rose".into()),
        contact: Some(Contact::from_uri(Uri::sip_user("bob", "10.0.0.2").with_port(5070))),
        ..Default::default()
    };
    let ok = generate_response(&read_request(&invite), 200, "OK", &opts).expect("the 200");
    worker.send_to(ok.image(), addr(PROXY)).await.expect("sent");
    settle().await;
    let answered = caller.try_recv().expect("the 200 reaches the caller").raw.to_vec();
    assert_relayed(&answered, "the 200");
    // The caller's route set: the 200's Record-Route, reversed (§12.1.2).
    let route: Vec<String> = String::from_utf8_lossy(&answered)
        .lines()
        .filter_map(|line| line.strip_prefix("Record-Route: "))
        .map(str::to_string)
        .rev()
        .collect();
    assert!(!route.is_empty(), "the proxy records its route");

    let mut bye = Vec::new();
    for (method, cseq) in [("ACK", 1), ("BYE", 2)] {
        let sent = request(method, cseq, &method.to_ascii_lowercase(), Some("b-rose"), &route);
        caller.send_to(&sent, addr(PROXY)).await.expect("sent");
        settle().await;
        bye = worker.try_recv().expect("the request reaches the worker").raw.to_vec();
        assert_relayed(&bye, method);
    }
    let bye_ok =
        generate_response(&read_request(&bye), 200, "OK", &GenerateResponseOpts::default())
            .expect("the BYE's 200");
    worker.send_to(bye_ok.image(), addr(PROXY)).await.expect("sent");
    settle().await;
    let ended = caller.try_recv().expect("the BYE's 200 reaches the caller").raw.to_vec();
    assert_relayed(&ended, "the BYE's 200");
    task.abort();
}
