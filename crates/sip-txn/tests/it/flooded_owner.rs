//! The owner loop under a flood that keeps its inbound queue non-empty: the
//! layer leaves its thread every cooperative budget of datagrams, so the TU
//! still runs and its final leaves while the flood lasts, and a timer that
//! comes due during the flood fires within one budget of datagrams.
//!
//! The flood holds the paused runtime busy, so the clock moves only where the
//! test advances it, and the test waits by yielding: each yield lets the
//! owner run one poll.

use crate::common;
use common::*;
use std::sync::Arc;

use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::{Flood, SimulatedSignalingNetwork};
use sip_txn::timers::{T1, TIMER_I};
use sip_txn::TransactionLayer;

const TRANSIT: u64 = 5;
const FLOODER: &str = "10.0.0.9:5555";
/// The copies of the flood: far more than the few budgets the call needs.
const FLOOD: u64 = 4_096;
/// The depth the flood holds the queue at.
const DEPTH: usize = 64;
/// Tokio's cooperative budget: the ready operations one task poll completes
/// before its next ready one returns pending.
const COOP_BUDGET: u64 = 128;

/// The copies of the flood the layer had taken when each `status` response
/// left it.
fn sent_at(flood: &Flood, status: u16) -> Vec<u64> {
    let parser = CustomParser::new();
    let is_status = |raw: &[u8]| matches!(parser.parse(raw), Ok(SipMessage::Response(r)) if r.status() == status);
    flood.sent().into_iter().filter(|(_, raw)| is_status(raw)).map(|(taken, _)| taken).collect()
}

async fn yield_until(mut done: impl FnMut() -> bool) {
    while !done() {
        tokio::task::yield_now().await;
    }
}

/// RFC 3261 §17.2.1: the INVITE server transaction of a declined call sends
/// its final and retransmits it on Timer G while a stray-response flood
/// (§17.1.3: each passed to the core) holds its queue non-empty; the ACK ends
/// it on Timer I.
#[tokio::test(start_paused = true)]
async fn a_flood_never_holds_back_the_tus_final_nor_its_timer_g() {
    let net = SimulatedSignalingNetwork::new(TRANSIT);
    let b2bua = net.bind_udp(BindUdpOpts::new(addr(B2BUA), DEPTH)).await.expect("bind b2bua");
    let peer = net.bind_udp(BindUdpOpts::new(addr(PEER), 1024)).await.expect("bind peer");
    let stray = response_bytes(180, "Ringing", "INVITE", "z9hG4bK-stray", "stray@unit", true);
    let (b2bua, flood) = Flood::over(b2bua, &stray, addr(FLOODER), FLOOD);
    let (txn, mut events) = TransactionLayer::spawn(b2bua, Arc::new(CustomParser::new()), config());

    let (branch, call_id) = ("z9hG4bK-flooded", "flooded-call");
    peer.send_to(&inbound_request("INVITE", branch, call_id, None), addr(B2BUA))
        .await
        .expect("send the INVITE");
    elapse_ms(60).await;
    assert!(events.try_recv().is_ok(), "the INVITE reached the TU");

    flood.start(DEPTH);
    yield_until(|| flood.taken() > 0).await;
    let asked_at = flood.taken();
    assert!(asked_at < FLOOD, "the TU runs while the flood lasts (at {asked_at} of {FLOOD})");
    let decline = parse_response(&response_bytes(603, "Decline", "INVITE", branch, call_id, true));
    txn.send_response(decline, addr(PEER)).await.expect("the final is sent");
    let [final_at] = sent_at(&flood, 603)[..] else { panic!("one 603 leaves") };
    assert!(
        final_at <= asked_at + COOP_BUDGET,
        "the final leaves within a budget of the TU's ask (at {final_at}, asked at {asked_at})"
    );

    tokio::time::advance(std::time::Duration::from_millis(T1)).await;
    let due_at = flood.taken();
    yield_until(|| sent_at(&flood, 603).len() == 2).await;
    let retransmitted_at = sent_at(&flood, 603)[1];
    assert!(
        retransmitted_at <= due_at + COOP_BUDGET && retransmitted_at < FLOOD,
        "Timer G fires within a budget of coming due (at {retransmitted_at}, due at {due_at})"
    );

    yield_until(|| !flood.lasts()).await;
    peer.send_to(&inbound_request("ACK", branch, call_id, Some("peer-tag")), addr(B2BUA))
        .await
        .expect("send the ACK");
    elapse_ms(TIMER_I + 100).await;
    assert_eq!(txn.metrics().active_transactions(), 0, "Timer I ends the transaction");
}
