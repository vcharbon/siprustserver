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
    /// Messages and cancellations the layer handed the TU for the case's
    /// two copies of its datagram: the preamble's and the follow-up's are
    /// not counted.
    pub handed_up: usize,
    /// Of those, the responses that matched a client transaction.
    pub matched: usize,
    /// Copies of the client INVITE the peer received over the whole case.
    pub invite_copies: usize,
    /// ACKs the peer received over the whole case.
    pub acks: usize,
}

/// What the TU has been handed so far.
#[derive(Default)]
struct Seen {
    handed_up: AtomicUsize,
    matched: AtomicUsize,
}

impl Seen {
    fn read(&self) -> (usize, usize) {
        (self.handed_up.load(Ordering::Relaxed), self.matched.load(Ordering::Relaxed))
    }
}

/// How long the TU rings an INVITE before its final: a CANCEL or PRACK sent
/// a datagram gap after the INVITE meets it ringing.
pub const FINAL_DELAY: Duration = Duration::from_secs(2);

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
        deferred_bound: Some(DeferredBound {
            normal: 64,
            emergency: 128,
            class_of: |_| InviteClass::Normal,
        }),
        invite_refusals: Some(InviteRefusals::new(refusal)),
        stateless_options: true,
        ..TransactionConfig::new(Arc::new(IdGen::seeded(0x1a5e)))
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
    let seen = Arc::new(Seen::default());
    let tu = tokio::spawn(answer_requests(layer.clone(), events, seen.clone()));

    for (request, kind) in seeds::client_requests() {
        if layer.send_request(request, addr(PEER), kind).await.is_err() {
            let failure = Some(Failure::OwnerEnded);
            return Outcome { failure, handed_up: 0, matched: 0, invite_copies: 0, acks: 0 };
        }
    }
    for before in &seed.before {
        let _ = peer.send_to(before, addr(LAYER)).await;
        tokio::time::sleep(RETRANSMIT_GAP).await;
    }
    if seed.release {
        let _ = layer.cancel_txns_for_call(seeds::INVITE_CALL_REF).await;
    }
    tokio::time::sleep(seed.settle).await;
    let (preamble_handed_up, preamble_matched) = seen.read();
    let _ = peer.send_to(datagram, addr(LAYER)).await;
    tokio::time::sleep(RETRANSMIT_GAP).await;
    let _ = peer.send_to(datagram, addr(LAYER)).await;
    tokio::time::sleep(RETRANSMIT_GAP).await;
    let (handed_up, matched) = seen.read();
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
    let received: Vec<Vec<u8>> = std::iter::from_fn(|| peer.try_recv()).map(|p| p.raw).collect();
    let invite_copies = received
        .iter()
        .filter(|raw| raw.starts_with(b"INVITE ") && contains(raw, b"z9hG4bK-lane-inv"))
        .count();
    let acks = received.iter().filter(|raw| raw.starts_with(b"ACK ")).count();
    Outcome {
        failure,
        handed_up: handed_up - preamble_handed_up,
        matched: matched - preamble_matched,
        invite_copies,
        acks,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The lane's TU: every request it is handed is answered through the layer,
/// an INVITE with a 180 then, [`FINAL_DELAY`] later, a final its Call-ID
/// picks (2xx, busy or unavailable), any other request but an ACK with a
/// 200. What it is handed is counted in `seen`.
async fn answer_requests(
    layer: TransactionLayer,
    mut events: mpsc::Receiver<TransactionEvent>,
    seen: Arc<Seen>,
) {
    while let Some(event) = events.recv().await {
        let (message, src, matched) = match event {
            TransactionEvent::Message { message, src, matched_client_txn } => {
                (message, src, matched_client_txn)
            }
            TransactionEvent::Cancelled { .. } => {
                seen.handed_up.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            _ => continue,
        };
        seen.handed_up.fetch_add(1, Ordering::Relaxed);
        if matched {
            seen.matched.fetch_add(1, Ordering::Relaxed);
        }
        let SipMessage::Request(req) = *message else { continue };
        let to_tag = req.to().tag().is_none().then(|| "lane-uas".to_owned());
        let opts = GenerateResponseOpts { to_tag, ..GenerateResponseOpts::default() };
        let (status, reason) = match *req.method() {
            Method::Ack => continue,
            Method::Invite => {
                if let Ok(ringing) = generate_response(&req, 180, "Ringing", &opts) {
                    let _ = layer.send_response(ringing, src).await;
                }
                let (status, reason) =
                    INVITE_FINALS[req.call_id().as_str().len() % INVITE_FINALS.len()];
                if let Ok(resp) = generate_response(&req, status, reason, &opts) {
                    let layer = layer.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(FINAL_DELAY).await;
                        let _ = layer.send_response(resp, src).await;
                    });
                }
                continue;
            }
            _ => (200, "OK"),
        };
        if let Ok(resp) = generate_response(&req, status, reason, &opts) {
            let _ = layer.send_response(resp, src).await;
        }
    }
}

/// The finals the TU answers an INVITE with, one picked per Call-ID: the
/// seeds' dialog (a 22-byte Call-ID) is answered 200.
const INVITE_FINALS: [(u16, &str); 3] =
    [(486, "Busy Here"), (200, "OK"), (503, "Service Unavailable")];
