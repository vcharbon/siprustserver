//! The ACK to a non-2xx final the proxy answered an INVITE with itself is
//! recognised by the final's To-tag, keyed by the deployment's shared cookie
//! key: it is absorbed with nothing remembered, at the proxy that sent the
//! final and at any other proxy holding the key (a restart, a VIP move).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::cancel_lru::CancelBranchLru;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::parse_refusals::ParseRefusals;
use crate::refusals::{Refusals, REFUSED_MEMO_MAX};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::security::own_final_tag::own_final_tagger;
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::strategy::{DecodeResult, RouteParams, SelectError, SelectOpts};
use crate::RoutingStrategy;

const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const W1: &str = "10.0.0.1";
const KEY: &[u8] = b"0123456789abcdef-shared-cookie-key";

/// A selection that always fails: every new INVITE draws the proxy's 503.
struct NoWorker;

#[async_trait]
impl RoutingStrategy for NoWorker {
    fn name(&self) -> &str {
        "NoWorker"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err(SelectError::NoTarget { reason: "no worker alive".into() })
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
    fn stickiness_primary<'p>(&self, _params: &'p RouteParams) -> Option<&'p str> {
        None
    }
}

async fn core(
    key: &[u8],
    strategy: Arc<dyn RoutingStrategy>,
    lru: Arc<CancelBranchLru>,
) -> ProxyCore {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let tagger = own_final_tagger(key, &sip_txn::IdGen::seeded(7));
    ProxyCoreBuilder::for_test(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .cancel_lru(lru)
        .refusals(Arc::new(Refusals::keyed(tagger.clone(), 0, REFUSED_MEMO_MAX)))
        .parse_refusals(Arc::new(ParseRefusals::keyed(tagger)))
        .build(ep)
}

fn forward_all() -> Arc<dyn RoutingStrategy> {
    Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)))
}

fn lru() -> Arc<CancelBranchLru> {
    Arc::new(CancelBranchLru::with_clock(Clock::test_at(0)))
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn request(method: &str, cseq_method: &str, to_tag: &str) -> SipMessage {
    parse(&format!(
        "{method} sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKown;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>{to_tag}\r\n\
Call-ID: own-final@test\r\n\
CSeq: 1 {cseq_method}\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn uac() -> SocketAddr {
    format!("{UAC}:5060").parse().unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_ack_to_an_own_503_is_absorbed_with_no_memo_at_every_proxy_sharing_the_key() {
    let refusing_lru = lru();
    let refusing = core(KEY, Arc::new(NoWorker), refusing_lru.clone()).await;
    let invite = request("INVITE", "INVITE", "");
    refusing.route_request_from(&invite, uac()).await;
    assert_eq!(refusing_lru.size(), 0, "a final the proxy answers itself writes no memo");

    let SipMessage::Request(req) = &invite else { unreachable!() };
    let ack = request("ACK", "ACK", &format!(";tag={}", refusing.refusals.to_tag(req)));
    let here = refusing.route_request_from(&ack, uac()).await;
    assert_eq!(here.target, None, "absorbed by the proxy that sent the 503");

    let peer_lru = lru();
    let peer = core(KEY, forward_all(), peer_lru.clone()).await;
    let there = peer.route_request_from(&ack, uac()).await;
    assert_eq!(there.target, None, "absorbed by another proxy holding the key");
    assert_eq!(peer_lru.size(), 0);

    let stranger = core(b"0123456789abcdef-another-key", forward_all(), lru()).await;
    let elsewhere = stranger.route_request_from(&ack, uac()).await;
    assert_eq!(
        elsewhere.target,
        Some(ProxyAddr::new(W1, 5060)),
        "a proxy under another key does not take the tag for its own"
    );
}
