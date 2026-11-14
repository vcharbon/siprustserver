//! `NewCallGate::refusing_at_age` reads the layer's inbound queue too: with
//! the consumer's switch off and no event waiting, a new normal INVITE is
//! refused at receipt once the oldest datagram of the inbound queue has waited
//! the gate's age, and admitted under it. Emergency and in-dialog INVITEs pass.
//! The endpoint states the wait the test sets ([`StatedWait`]).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{CustomParser, SipMessage, SipRequest, SipResponse};
use sip_net::{
    BindUdpOpts, SendError, SignalingNetwork, UdpEndpoint, UdpEndpointCounters, UdpPacket,
};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::{
    IdGen, InviteClass, InviteRefusals, NewCallGate, TransactionConfig, TransactionEvent,
    TransactionLayer,
};

use crate::common::{self, addr, count_responses, elapse_ms, inbound_request, Stack, B2BUA, PEER};

/// The age the gate refuses at.
const AGE_MS: u64 = 450;

/// An endpoint whose inbound queue reads the wait, ms, the test stores.
struct StatedWait {
    inner: Box<dyn UdpEndpoint>,
    wait_ms: Arc<AtomicU64>,
}

#[async_trait]
impl UdpEndpoint for StatedWait {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.inner.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        self.inner.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.inner.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
    }
    fn queue_wait(&self) -> Duration {
        Duration::from_millis(self.wait_ms.load(Ordering::Relaxed))
    }
    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.inner.counters()
    }
}

/// A stateless refusal under a request-derived To-tag.
fn refuse_503(req: &SipRequest) -> Result<SipResponse, sip_message::generators::GenerateError> {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(format!("refused-{}", req.call_id().as_str().replace('@', "."))),
            ..Default::default()
        },
    )
}

/// A layer whose gate's switch stays off, refusing at [`AGE_MS`], over an
/// endpoint stating the wait the returned handle holds.
async fn stack() -> (Stack, Arc<AtomicU64>) {
    let net = SimulatedSignalingNetwork::new(1);
    let bound = net.bind_udp(BindUdpOpts::new(addr(B2BUA), 1024)).await.expect("bind b2bua");
    let peer = net.bind_udp(BindUdpOpts::new(addr(PEER), 1024)).await.expect("bind peer");
    let wait_ms = Arc::new(AtomicU64::new(0));
    let endpoint = Box::new(StatedWait { inner: bound, wait_ms: wait_ms.clone() });
    let gate = NewCallGate::new(Arc::new(AtomicBool::new(false)), sip_txn::class_of)
        .refusing_at_age(Duration::from_millis(AGE_MS));
    let config = TransactionConfig {
        id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
        invite_refusals: Some(InviteRefusals::new(Arc::new(refuse_503))),
        new_call_gate: Some(gate),
        ..common::config()
    };
    let (txn, events) = TransactionLayer::spawn(endpoint, Arc::new(CustomParser::new()), config);
    (Stack { txn, events, peer, net }, wait_ms)
}

/// An INVITE with `extra` header lines.
fn invite(branch: &str, call_id: &str, to_tag: Option<&str>, extra: &str) -> Vec<u8> {
    let raw = String::from_utf8(inbound_request("INVITE", branch, call_id, to_tag)).unwrap();
    raw.replacen("Content-Length: 0\r\n", &format!("{extra}Content-Length: 0\r\n"), 1).into_bytes()
}

fn invites(events: &[TransactionEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(e, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Request(r) if r.method() == "INVITE"))
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_new_invite_is_refused_once_the_inbound_queue_waited_the_age() {
    let (mut stack, wait_ms) = stack().await;

    wait_ms.store(AGE_MS - 1, Ordering::Relaxed);
    stack.inject(&invite("z9hG4bK-young", "young@unit", None, "")).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 503), 0, "under the age: admitted");
    assert_eq!(invites(&stack.drain_events()), 1);

    wait_ms.store(AGE_MS, Ordering::Relaxed);
    stack.inject(&invite("z9hG4bK-late", "late@unit", None, "")).await;
    let sos = invite("z9hG4bK-sos", "sos@unit", None, "Resource-Priority: esnet.0\r\n");
    stack.inject(&sos).await;
    stack.inject(&invite("z9hG4bK-re", "re@unit", Some("b2bua-tag"), "")).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 503), 1, "the normal INVITE is refused");
    assert_eq!(invites(&stack.drain_events()), 2, "the emergency and in-dialog INVITEs pass");
    assert_eq!(stack.txn.metrics().new_call_gate_refused(InviteClass::Normal), 1);
    assert_eq!(stack.txn.metrics().inbound_queue_delay(), Duration::from_millis(AGE_MS));

    wait_ms.store(0, Ordering::Relaxed);
    stack.inject(&invite("z9hG4bK-after", "after@unit", None, "")).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 503), 0, "a drained queue admits again");
    assert_eq!(invites(&stack.drain_events()), 1);
}
