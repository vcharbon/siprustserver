//! A datagram the parser panics on costs that datagram only: the owner loop
//! every transaction shares drops it, counts it, and keeps serving.

use std::sync::Arc;

use sip_message::{CustomParser, SipMessage, SipParseError, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::{TransactionConfig, TransactionEvent, TransactionLayer};

use crate::common::{addr, elapse_ms, inbound_request, B2BUA, PEER};

/// The datagram that makes [`PanicsOnMarker`] panic.
const MARKER: &[u8] = b"PANIC";

/// The real parser, except that it panics on a datagram opening with
/// [`MARKER`]: a parser defect reached by one input.
struct PanicsOnMarker(CustomParser);

impl SipParser for PanicsOnMarker {
    fn name(&self) -> &str {
        "panics-on-marker"
    }

    fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError> {
        if raw.starts_with(MARKER) {
            panic!("probe: parser panic");
        }
        self.0.parse(raw)
    }
}

#[tokio::test(start_paused = true)]
async fn a_datagram_the_parser_panics_on_is_dropped_and_the_owner_keeps_serving() {
    let net = SimulatedSignalingNetwork::new(1);
    let endpoint = net.bind_udp(BindUdpOpts::new(addr(B2BUA), 64)).await.expect("bind");
    let peer = net.bind_udp(BindUdpOpts::new(addr(PEER), 64)).await.expect("bind peer");
    let parser = Arc::new(PanicsOnMarker(CustomParser::new()));
    let (txn, mut events) = TransactionLayer::spawn(endpoint, parser, TransactionConfig::default());

    peer.send_to(MARKER, addr(B2BUA)).await.expect("the marker leaves");
    let options = inbound_request("OPTIONS", "z9hG4bK-after-panic", "after-panic@u", None);
    peer.send_to(&options, addr(B2BUA)).await.expect("the OPTIONS leaves");
    elapse_ms(10).await;

    assert!(!txn.owner_task().is_finished(), "the owner loop outlives the parser panic");
    assert_eq!(txn.metrics().parse_panics(), 1, "the panicking datagram is counted");
    assert_eq!(txn.metrics().parse_errors(), 0, "a panic is no parse error");
    match events.try_recv() {
        Ok(TransactionEvent::Message { message, .. }) => {
            assert!(matches!(*message, SipMessage::Request(ref r) if r.method() == "OPTIONS"));
        }
        other => panic!("the datagram after the panic reaches the consumer, got {other:?}"),
    }
}
