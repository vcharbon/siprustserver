//! The owner loop's pulse tells a loop parked on one turn from a loop idle on
//! its socket: only the first reads stalled.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_message::CustomParser;
use sip_net::{
    BindUdpOpts, SendError, SignalingNetwork, UdpEndpoint, UdpEndpointCounters, UdpPacket,
};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::{TransactionConfig, TransactionEvent, TransactionLayer};
use tokio::sync::mpsc;

use crate::common::{addr, B2BUA, PEER};

const THRESHOLD: Duration = Duration::from_secs(5);

/// An endpoint whose sends never complete: a turn that sends parks on it.
struct ParkingSends(Box<dyn UdpEndpoint>);

#[async_trait]
impl UdpEndpoint for ParkingSends {
    async fn send_to(&self, _: &[u8], _: SocketAddr) -> Result<(), SendError> {
        std::future::pending().await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        self.0.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.0.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        self.0.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.0.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.0.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.0.counters()
    }
}

/// The layer over an endpoint, `parking` its sends or not, and the consumer
/// half of its event queue, held open for the layer's life.
async fn layer(parking: bool) -> (TransactionLayer, mpsc::Receiver<TransactionEvent>) {
    let net = SimulatedSignalingNetwork::new(1);
    let bound = net.bind_udp(BindUdpOpts::new(addr(B2BUA), 64)).await.expect("bind");
    let endpoint: Box<dyn UdpEndpoint> =
        if parking { Box::new(ParkingSends(bound)) } else { bound };
    let parser = Arc::new(CustomParser::new());
    TransactionLayer::spawn(endpoint, parser, TransactionConfig::default())
}

#[tokio::test(start_paused = true)]
async fn an_idle_owner_is_never_stalled() {
    let (txn, _events) = layer(false).await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(txn.owner_pulse().busy_for(), None, "waiting on its socket");
    assert!(!txn.owner_task().is_finished());
}

#[tokio::test(start_paused = true)]
async fn an_owner_parked_on_one_turn_reads_stalled_past_the_threshold() {
    let (txn, _events) = layer(true).await;
    let sender = txn.clone();
    tokio::spawn(async move { sender.send_raw(b"OPTIONS".to_vec(), addr(PEER)).await });
    tokio::time::sleep(THRESHOLD).await;
    assert!(txn.owner_pulse().busy_for().is_some(), "on its turn");
    assert!(!txn.owner_pulse().stalled(THRESHOLD), "not past the threshold yet");
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(txn.owner_pulse().stalled(THRESHOLD));
    assert!(!txn.owner_task().is_finished(), "parked, not ended");
}

#[tokio::test(start_paused = true)]
async fn an_aborted_owner_has_ended() {
    let (txn, _events) = layer(false).await;
    txn.abort_owner();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(txn.owner_task().is_finished());
}
