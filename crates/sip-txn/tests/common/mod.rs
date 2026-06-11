//! Shared scaffolding for the transaction-layer tests: a simulated-fabric
//! stack (b2bua endpoint under test + a peer endpoint to inject from) and a
//! couple of raw-message builders. Mirrors the source tests' `fakeStack` /
//! simulated `SignalingNetwork` wiring.
//!
//! ## Time idiom — one and only one
//!
//! Every test runs under `#[tokio::test(start_paused = true)]`. The single way
//! these tests move time is to **await**: `tokio::time::sleep(d).await` (or
//! `events.recv().await` / `peer.recv().await`). While the test task is parked,
//! tokio's paused-clock **auto-advance** fast-forwards the clock to the next
//! pending deadline — running the owner's retransmit/timeout/cleanup timers and
//! the simulated fabric's transit sleeps, in order, instantly — and only wakes
//! the test once everything due before `d` has run to quiescence. So a 32 s
//! Timer B test costs ~no wall-clock and needs no manual stepping.
//!
//! There is deliberately **no** `advance`/`pump`/`settle` helper here: a single
//! big `tokio::time::advance` skips timers registered mid-advance (a foot-gun),
//! and a busy `yield_now` loop never lets the runtime go idle (so auto-advance
//! never triggers). Await, then read state with the `drain_*` helpers below.
//! (When a test genuinely needs to observe state *between* deadlines, use
//! `sip_clock::testkit::advance_in_chunks` — not a bespoke local pump.)

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{CustomParser, SipMessage, SipParser, SipRequest};
use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};
use sip_txn::{IdGen, TransactionConfig, TransactionEvent, TransactionLayer};
use tokio::sync::mpsc;

pub const B2BUA: &str = "127.0.0.1:5070";
pub const PEER: &str = "10.0.0.1:5555";

pub fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// A bound transaction layer over the simulated fabric, plus a peer endpoint
/// for injecting wire bytes at it.
pub struct Stack {
    pub txn: TransactionLayer,
    pub events: mpsc::Receiver<TransactionEvent>,
    pub peer: Box<dyn UdpEndpoint>,
    pub net: SimulatedSignalingNetwork,
}

impl Stack {
    /// `transit_ms` = simulated per-hop delay; `udp_queue_max` sizes the event
    /// queue (`max(64, ×4)`); `b2bua_queue` sizes the b2bua recv queue.
    pub async fn build(transit_ms: u64, udp_queue_max: usize, b2bua_queue: usize) -> Stack {
        let net = SimulatedSignalingNetwork::new(transit_ms);
        let b2bua_ep = net
            .bind_udp(BindUdpOpts::new(addr(B2BUA), b2bua_queue))
            .await
            .expect("bind b2bua");
        let peer = net
            .bind_udp(BindUdpOpts::new(addr(PEER), 1024))
            .await
            .expect("bind peer");

        let parser = Arc::new(CustomParser::new());
        let (txn, events) = TransactionLayer::spawn(
            b2bua_ep,
            parser,
            TransactionConfig {
                udp_queue_max,
                // Deterministic ids so any tag/branch fabrication is stable.
                id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            },
        );

        Stack { txn, events, peer, net }
    }

    /// Inject raw bytes from the peer toward the b2bua port.
    pub async fn inject(&self, raw: &[u8]) {
        self.peer.send_to(raw, addr(B2BUA)).await.expect("inject");
    }

    /// Drain whatever events are currently buffered (non-blocking).
    pub fn drain_events(&mut self) -> Vec<TransactionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Drain + parse everything the peer endpoint has received from the b2bua
    /// (the outbound side: 100/200/487 responses, retransmitted requests,
    /// auto-ACKs). Non-blocking — call after [`elapse_ms`].
    pub fn drain_peer(&self) -> Vec<SipMessage> {
        let mut out = Vec::new();
        while let Some(p) = self.peer.try_recv() {
            if let Ok(m) = CustomParser::new().parse(&p.raw) {
                out.push(m);
            }
        }
        out
    }
}

/// Sugar for the one time idiom: park the test for `ms` of virtual time, which
/// (under `start_paused`) auto-advances through every timer + transit sleep due
/// before then, leaving the owner quiesced. Read state afterwards with `drain_*`.
pub async fn elapse_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

pub fn parse_request(raw: &str) -> SipRequest {
    let bytes = raw.replace('\n', "\r\n");
    match CustomParser::new().parse(bytes.as_bytes()).expect("parse request") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected request"),
    }
}

pub fn parse_response(raw: &[u8]) -> sip_message::SipResponse {
    match CustomParser::new().parse(raw).expect("parse response") {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => panic!("expected response"),
    }
}

/// Count requests of `method` / responses of `status` in a drained batch.
pub fn count_requests(msgs: &[SipMessage], method: &str) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, SipMessage::Request(r) if r.method == method))
        .count()
}
pub fn count_responses(msgs: &[SipMessage], status: u16) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, SipMessage::Response(r) if r.status == status))
        .count()
}

/// An inbound request *from the peer toward the b2bua*. Fixed From-tag
/// `caller-tag` so an INVITE and a follow-up CANCEL/ACK share dialog identity.
pub fn inbound_request(method: &str, branch: &str, call_id: &str, to_tag: Option<&str>) -> Vec<u8> {
    let to = match to_tag {
        Some(t) => format!("<sip:b2bua@127.0.0.1:5070>;tag={t}"),
        None => "<sip:b2bua@127.0.0.1:5070>".to_string(),
    };
    format!(
        "{method} sip:b2bua@127.0.0.1:5070 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {method}\r\n\
         Contact: <sip:caller@10.0.0.1:5555>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// INVITE / BYE outbound request with the given Via branch (handles test).
pub fn outbound_request(method: &str, branch: &str) -> SipRequest {
    let to = if method == "BYE" {
        "<sip:bob@192.0.2.20:5060>;tag=remote-bob"
    } else {
        "<sip:bob@192.0.2.20:5060>"
    };
    parse_request(&format!(
        "{method} sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15070;branch={branch}\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15070>;tag=b2bua-tag\n\
         To: {to}\n\
         Call-ID: handle-shape-test\n\
         CSeq: 1 {method}\n\
         Contact: <sip:b2bua@127.0.0.1:15070>\n\
         Content-Length: 0\n\n"
    ))
}

/// An in-dialog re-INVITE (carries a To-tag) — keeps the 32 s Timer B, unlike an
/// out-of-dialog initial INVITE which gets the long no-answer-window backstop.
pub fn outbound_reinvite(branch: &str) -> SipRequest {
    parse_request(&format!(
        "INVITE sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15070;branch={branch}\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15070>;tag=b2bua-tag\n\
         To: <sip:bob@192.0.2.20:5060>;tag=remote-bob\n\
         Call-ID: reinvite-test\n\
         CSeq: 2 INVITE\n\
         Contact: <sip:b2bua@127.0.0.1:15070>\n\
         Content-Length: 0\n\n"
    ))
}

/// INVITE carrying Via `cr`/`lg` custom params (cancel-on-evict test).
pub fn invite_with_cr_lg(call_ref: &str, call_id: &str, branch: &str, leg_id: &str) -> SipRequest {
    parse_request(&format!(
        "INVITE sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch={branch};cr={call_ref};lg={leg_id}\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-{leg_id}\n\
         To: <sip:bob@192.0.2.20:5060>\n\
         Call-ID: {call_id}\n\
         CSeq: 1 INVITE\n\
         Contact: <sip:b2bua@127.0.0.1:15071>\n\
         Content-Length: 0\n\n"
    ))
}

/// A response from the peer toward the b2bua (absorb / bounded-queue tests).
pub fn response_bytes(
    status: u16,
    reason: &str,
    cseq_method: &str,
    branch: &str,
    call_id: &str,
    with_to_tag: bool,
) -> Vec<u8> {
    let to = if with_to_tag {
        "<sip:peer@10.0.0.1:5555>;tag=peer-tag"
    } else {
        "<sip:peer@10.0.0.1:5555>"
    };
    format!(
        "SIP/2.0 {status} {reason}\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         From: <sip:b2bua@127.0.0.1:5070>;tag=b2bua-tag\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {cseq_method}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}
