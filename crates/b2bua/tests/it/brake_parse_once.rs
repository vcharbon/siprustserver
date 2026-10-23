//! A datagram the ingress brake judged is parsed once: the transaction layer
//! behind the brake takes the brake's parse and does not parse it again.
//!
//! The production brake ([`build_ingress_brake_hook`]) sits on a simulated
//! bind, engaged on every datagram (threshold 0). The layer behind it parses
//! with a counting parser, so every parse the layer makes is visible. The
//! peer places an emergency INVITE (the brake admits it), the TU answers 486,
//! and the peer ACKs it: a whole transaction, ended by Timer I.
//!
//! Clock: `#[tokio::test(start_paused = true)]`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::admission::Refusals;
use b2bua::ingress_brake::{build_ingress_brake_hook, IngressBrakeConfig, IngressBrakeCounters};
use sip_message::generators::ack::generate_ack_for_non_2xx;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{serialize, Bytes, CustomParser, SipMessage, SipParseError, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent, TransactionLayer};

const TRANSIT_MS: u64 = 15;
const QUEUE_MAX: usize = 8;

fn b2bua_addr() -> SocketAddr {
    "192.0.2.20:5060".parse().unwrap()
}

fn peer_addr() -> SocketAddr {
    "192.0.2.10:5060".parse().unwrap()
}

/// The pipeline's parser, counting every datagram it is given.
struct Counting {
    inner: CustomParser,
    parses: Arc<AtomicUsize>,
}

impl SipParser for Counting {
    fn name(&self) -> &str {
        "counting"
    }

    fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError> {
        self.parses.fetch_add(1, Ordering::Relaxed);
        self.inner.parse(raw)
    }

    fn parse_shared(&self, raw: Bytes) -> Result<SipMessage, SipParseError> {
        self.parses.fetch_add(1, Ordering::Relaxed);
        self.inner.parse_shared(raw)
    }
}

const EMERGENCY_INVITE: &[u8] = b"INVITE sip:911@192.0.2.20:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-once-1\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=alice-once\r\n\
To: <sip:911@example.com>\r\n\
Call-ID: parse-once@192.0.2.10\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@192.0.2.10:5060>\r\n\
Resource-Priority: esnet.0\r\n\
Content-Length: 0\r\n\r\n";

#[tokio::test(start_paused = true)]
async fn a_datagram_the_brake_parsed_is_not_parsed_again_by_the_transaction_layer() {
    let net = SimulatedSignalingNetwork::new(TRANSIT_MS);
    let counters = IngressBrakeCounters::new();
    let hook = build_ingress_brake_hook(
        IngressBrakeConfig { queue_max: QUEUE_MAX, threshold_pct: 0 },
        counters.clone(),
        Refusals::new(5, 0, 64, &IdGen::seeded(11)),
    );
    let endpoint = net
        .bind_udp(BindUdpOpts::new(b2bua_addr(), QUEUE_MAX).with_pre_ingress(hook))
        .await
        .expect("bind the b2bua");
    let peer = net.bind_udp(BindUdpOpts::new(peer_addr(), 64)).await.expect("bind the peer");
    let parses = Arc::new(AtomicUsize::new(0));
    let parser = Counting { inner: CustomParser::new(), parses: parses.clone() };
    let (layer, mut events) = TransactionLayer::spawn(
        endpoint,
        Arc::new(parser),
        TransactionConfig::new(Arc::new(IdGen::seeded(7))),
    );

    peer.send_to(EMERGENCY_INVITE, b2bua_addr()).await.expect("send the INVITE");
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("the INVITE reaches the TU")
        .expect("the layer is up");
    let TransactionEvent::Message { message, src, .. } = event else {
        panic!("the INVITE is handed up as a message: {event:?}");
    };
    let SipMessage::Request(invite) = *message else { panic!("a request") };
    assert_eq!(counters.emergency_bypassed(), 1, "the brake judged the INVITE");
    assert_eq!(parses.load(Ordering::Relaxed), 0, "the layer parsed the brake's INVITE again");

    let opts = GenerateResponseOpts { to_tag: Some("b2bua-once".into()), ..Default::default() };
    let busy = generate_response(&invite, 486, "Busy Here", &opts).expect("a 486");
    layer.send_response(busy.clone(), src).await.expect("send the 486");
    tokio::time::sleep(Duration::from_millis(TRANSIT_MS * 2)).await;
    let ack = generate_ack_for_non_2xx(&invite, &busy, &[]).expect("the ACK");
    peer.send_to(&serialize(&SipMessage::Request(ack)), b2bua_addr()).await.expect("send the ACK");
    // Past Timer I (T4 on UDP): the server transaction ends.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let received: Vec<Vec<u8>> =
        std::iter::from_fn(|| peer.try_recv()).map(|p| p.raw.to_vec()).collect();
    assert!(
        received.iter().any(|raw| raw.starts_with(b"SIP/2.0 486 ")),
        "the peer received the 486"
    );
    assert_eq!(layer.metrics().active_transactions(), 0, "the transaction ended");
    assert_eq!(parses.load(Ordering::Relaxed), 0, "the layer parsed a datagram the brake parsed");
}
