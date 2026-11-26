//! A proxy advertising an IPv6 address, configured as an RFC 3261 `hostport`
//! (`[2001:db8::1]:5060`): the Via and Record-Route it stamps name it
//! bracketed once (§19.1.1, §20.42).

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::header::HeaderValue;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::core::ProxyCoreBuilder;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::RoutingStrategy;

const CALLER: &str = "198.51.100.10";
const PROXY_NODE: &str = "192.0.2.250";
const ADVERTISED: &str = "[2001:db8::1]:5060";
const WORKER: &str = "10.0.0.1";

#[tokio::test(start_paused = true)]
async fn an_ipv6_advertised_address_is_stamped_bracketed_once() {
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |host: &str| BindUdpOpts::new(format!("{host}:5060").parse().unwrap(), 64);
    let ep = net.bind_udp(bind(PROXY_NODE)).await.unwrap();
    let worker = net.bind_udp(bind(WORKER)).await.unwrap();
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(WORKER, 5060)));
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let advertised = ProxyAddr::parse(ADVERTISED).expect("a bracketed IPv6 hostport");
    let core =
        ProxyCoreBuilder::for_test(advertised, strategy, reg).clock(Clock::test_at(0)).build(ep);

    let invite = CustomParser::new()
        .parse(
            format!(
                "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLER}:5060;branch=z9hG4bKv6adv;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: v6-advertised@example.com\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{CALLER}:5060>\r\n\
Content-Length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    core.route_request_from(&invite, format!("{CALLER}:5060").parse().unwrap()).await;

    let pkt = worker.recv().await.expect("the INVITE reaches the worker");
    let SipMessage::Request(forwarded) = CustomParser::new().parse(&pkt.raw).unwrap() else {
        panic!("a forwarded request")
    };
    let pushed = forwarded.top_via().to_wire();
    assert!(pushed.starts_with(&format!("SIP/2.0/UDP {ADVERTISED};")), "{pushed}");
    let text = String::from_utf8_lossy(&pkt.raw);
    let stamped: Vec<&str> = text.lines().filter(|l| l.starts_with("Record-Route:")).collect();
    assert!(!stamped.is_empty(), "{text}");
    for rr in stamped {
        assert!(rr.contains(&format!("<sip:{ADVERTISED};")), "{rr}");
    }
}
