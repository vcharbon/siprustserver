//! One case of the lane: a transaction layer over the simulated fabric on its
//! own paused-clock runtime, configured as a B2BUA worker configures it
//! (shared refusals, a deferred-backlog bound, stateless OPTIONS), fed one
//! datagram and run past the last timer it can arm.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{CustomParser, Method, SipMessage};
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::{
    DeferredBound, IdGen, InviteClass, InviteRefusals, TransactionConfig, TransactionEvent,
    TransactionLayer,
};
use tokio::sync::mpsc;

use super::seeds::{self, Seed, LAYER, PEER};

/// How a case failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The owner task ended: a panic on the loop every transaction shares.
    OwnerEnded,
    /// The parse guard caught a parser panic.
    ParserPanicked,
}

/// What a case did: its failure, and how much of the layer it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub failure: Option<Failure>,
    /// Messages the layer handed the TU.
    pub handed_up: usize,
}

/// How long a case runs after its last datagram: past the INVITE bound
/// (158 s), every Timer B/F/H/J/K/L and the safety-net sweep.
const RUN_PAST_TIMERS: Duration = Duration::from_secs(200);

/// The gap between a datagram and its retransmission: a UDP peer's first T1.
const RETRANSMIT_GAP: Duration = Duration::from_millis(500);

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("a lane address")
}

/// The configuration a B2BUA worker runs the layer with, small queues.
fn config() -> TransactionConfig {
    let refusal: sip_txn::NewCallRefusal = Arc::new(|req| {
        generate_response(req, 503, "Service Unavailable", &GenerateResponseOpts::default())
    });
    TransactionConfig {
        udp_queue_max: 64,
        id_gen: Arc::new(IdGen::seeded(0x1a5e)),
        deferred_bound: Some(DeferredBound {
            normal: 64,
            emergency: 128,
            class_of: |_| InviteClass::Normal,
        }),
        invite_refusals: Some(InviteRefusals::new(refusal)),
        stateless_options: true,
        ..TransactionConfig::default()
    }
}

/// Runs `datagram` as the mutated copy of `seed` on a fresh runtime.
pub fn case(seed: &Seed, datagram: &[u8]) -> Option<Failure> {
    outcome(seed, datagram).failure
}

/// [`case`], with how far it reached.
pub fn outcome(seed: &Seed, datagram: &[u8]) -> Outcome {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("the lane runtime");
    runtime.block_on(drive(seed, datagram))
}

async fn drive(seed: &Seed, datagram: &[u8]) -> Outcome {
    let net = SimulatedSignalingNetwork::new(1);
    let endpoint = net.bind_udp(BindUdpOpts::new(addr(LAYER), 256)).await.expect("bind the layer");
    let peer = net.bind_udp(BindUdpOpts::new(addr(PEER), 4096)).await.expect("bind the peer");
    let (layer, events) =
        TransactionLayer::spawn(endpoint, Arc::new(CustomParser::new()), config());
    let handed_up = Arc::new(AtomicUsize::new(0));
    let tu = tokio::spawn(answer_requests(layer.clone(), events, handed_up.clone()));

    for (request, kind) in seeds::client_requests() {
        if layer.send_request(request, addr(PEER), kind).await.is_err() {
            return Outcome { failure: Some(Failure::OwnerEnded), handed_up: 0 };
        }
    }
    for before in &seed.before {
        let _ = peer.send_to(before, addr(LAYER)).await;
        tokio::time::sleep(RETRANSMIT_GAP).await;
    }
    let _ = peer.send_to(datagram, addr(LAYER)).await;
    tokio::time::sleep(RETRANSMIT_GAP).await;
    let _ = peer.send_to(datagram, addr(LAYER)).await;
    tokio::time::sleep(RETRANSMIT_GAP).await;
    for after in &seed.after {
        let _ = peer.send_to(after, addr(LAYER)).await;
        tokio::time::sleep(RETRANSMIT_GAP).await;
    }
    tokio::time::sleep(RUN_PAST_TIMERS).await;

    let failure = if layer.owner_task().is_finished() {
        Some(Failure::OwnerEnded)
    } else if layer.metrics().parse_panics() > 0 {
        Some(Failure::ParserPanicked)
    } else {
        None
    };
    tu.abort();
    Outcome { failure, handed_up: handed_up.load(Ordering::Relaxed) }
}

/// The lane's TU: every request it is handed is answered through the layer,
/// an INVITE with a 180 then a final its Call-ID picks (2xx, busy or
/// unavailable), any other request but an ACK with a 200. Every message
/// handed up is counted in `handed_up`.
async fn answer_requests(
    layer: TransactionLayer,
    mut events: mpsc::Receiver<TransactionEvent>,
    handed_up: Arc<AtomicUsize>,
) {
    while let Some(event) = events.recv().await {
        let TransactionEvent::Message { message, src, .. } = event else { continue };
        handed_up.fetch_add(1, Ordering::Relaxed);
        let SipMessage::Request(req) = *message else { continue };
        let to_tag = req.to().tag().is_none().then(|| "lane-uas".to_owned());
        let opts = GenerateResponseOpts { to_tag, ..GenerateResponseOpts::default() };
        let (status, reason) = match *req.method() {
            Method::Ack => continue,
            Method::Invite => {
                if let Ok(ringing) = generate_response(&req, 180, "Ringing", &opts) {
                    let _ = layer.send_response(ringing, src).await;
                }
                INVITE_FINALS[req.call_id().as_str().len() % INVITE_FINALS.len()]
            }
            _ => (200, "OK"),
        };
        if let Ok(resp) = generate_response(&req, status, reason, &opts) {
            let _ = layer.send_response(resp, src).await;
        }
    }
}

/// The finals the TU answers an INVITE with, one picked per Call-ID.
const INVITE_FINALS: [(u16, &str); 3] =
    [(200, "OK"), (486, "Busy Here"), (503, "Service Unavailable")];
