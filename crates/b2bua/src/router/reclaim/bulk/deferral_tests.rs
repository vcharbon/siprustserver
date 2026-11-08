//! The bulk reclaim never waits on a call's lock: a call whose lock a turn
//! holds is parked on a turn of its own queue and armed by a turn queued
//! behind it, while the router loop goes on serving. What reaches the
//! deferred call meanwhile, its caller's INVITE retransmission and a request
//! naming no `callRef` of ours, queues behind its park and is served by the
//! reclaimed call.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::config::B2buaConfig;
use crate::decision::test_adapter::reject;
use crate::decision::ScriptedDecisionEngine;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{
    answered, caller_bye, heard, invite, node_deciding, node_on, put_body, src, Node, NODE_SIP_ADDR,
};
use crate::router::ReplCommand;
use crate::store::PartitionRole;

const PRI: PartitionRole = PartitionRole::Primary;
const PEER: &str = "10.0.0.1:5060";

fn send(n: &Node, cmd: ReplCommand) {
    n.core.fail_back_sender().expect("a wired node").send(cmd).unwrap();
}

/// An answered call of `w0`'s own in its `pri:` partition, not live: what a
/// reboot reclaims.
async fn stored(n: &Node, cid: &str) -> call::Call {
    let call = answered("w0", "w1", cid);
    put_body(&n.store, PRI, "w0", &call).await;
    call
}

/// Whether `call_ref` is live here and armed.
fn armed(n: &Node, call_ref: &str) -> bool {
    let state = &n.core.router_ctx().state;
    state.contains(call_ref) && !state.is_parked(call_ref)
}

/// The caller hangs `call` up: answered, and its one CDR written.
async fn hang_up(n: &Node, caller: &dyn UdpEndpoint, call: &call::Call) {
    caller.send_to(&caller_bye(call), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let lines = heard(caller, Duration::from_millis(500)).await;
    assert_eq!(lines, ["SIP/2.0 200 OK"], "the BYE is answered");
    let cdrs = n.cdr.snapshot().into_iter().filter(|c| c.call_ref == call.call_ref).count();
    assert_eq!(cdrs, 1, "the call's one CDR");
}

/// Nothing of the reclaim is left once its calls ended.
fn nothing_left(n: &Node) {
    let ctx = n.core.router_ctx();
    assert_eq!(ctx.state.active_count(), 0, "no call is left");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
    assert_eq!(n.metrics.timer_live(), 0, "no timer is left behind");
}

/// An out-of-dialog OPTIONS from the proxy.
fn options() -> Vec<u8> {
    format!(
        "OPTIONS sip:127.0.0.2:5080 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {PEER};branch=z9hG4bK-keepalive\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:proxy@10.0.0.1>;tag=p\r\n\
         To: <sip:127.0.0.2:5080>\r\n\
         Call-ID: keepalive@10.0.0.1\r\n\
         CSeq: 1 OPTIONS\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_bulk_park_meeting_a_held_call_lock_leaves_the_loop_serving() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w0", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let proxy = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let call = stored(&n, "held").await;
    let held = ctx.state.lock(&call.call_ref).await;

    send(&n, ReplCommand::ReclaimAll);
    sip_clock::testkit::settle().await;
    proxy.send_to(&options(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let lines = heard(proxy.as_ref(), Duration::from_millis(100)).await;
    assert_eq!(lines.len(), 1, "the OPTIONS is answered while the call's lock is held: {lines:?}");
    assert!(!ctx.state.contains(&call.call_ref), "the held call waits for its park");

    drop(held);
    sip_clock::testkit::settle().await;
    assert!(armed(&n, &call.call_ref), "parked, then armed, once its lock is free");
    hang_up(&n, caller.as_ref(), &call).await;
    nothing_left(&n);
}

/// A peer's BYE naming call `c`, which no call is: its turn takes a permit
/// and waits on `c`'s lock.
fn bye_of_c() -> Vec<u8> {
    format!(
        "BYE sip:b2bua@127.0.0.2:5080;callRef=c SIP/2.0\r\n\
         Via: SIP/2.0/UDP {PEER};branch=z9hG4bK-permit\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@10.0.0.1>;tag=a\r\n\
         To: <sip:bob@127.0.0.2>;tag=b\r\n\
         Call-ID: permit@unit\r\n\
         CSeq: 2 BYE\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_deferred_park_waiting_for_its_permit_parks_and_arms_once_it_runs() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w0", |c| c.event_dispatch_concurrency = 1, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let call = stored(&n, "permit").await;
    let held = ctx.state.lock(&call.call_ref).await;
    // The one permit is taken by a turn waiting on another lock the test holds.
    let other = ctx.state.lock("c").await;
    peer.send_to(&bye_of_c(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(ctx.dispatcher.has_queue("c"), "the BYE's turn waits, its permit held");

    send(&n, ReplCommand::ReclaimAll);
    sip_clock::testkit::settle().await;
    drop(held);
    sip_clock::testkit::settle().await;
    assert!(!ctx.state.contains(&call.call_ref), "its park waits for a permit, its lock free");

    drop(other);
    sip_clock::testkit::settle().await;
    let lines = heard(peer.as_ref(), Duration::from_millis(100)).await;
    assert_eq!(lines, ["SIP/2.0 481 Call/Transaction Does Not Exist"]);
    assert!(armed(&n, &call.call_ref), "parked, then armed by the turn queued behind its park");
    hang_up(&n, caller.as_ref(), &call).await;
    nothing_left(&n);
}

#[tokio::test(start_paused = true)]
async fn two_bulk_reclaims_behind_a_closed_gate_arm_every_deferred_call_once_it_opens() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w0", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    n.membership.set_conditions("w0", false, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (deferred, free) = (stored(&n, "deferred").await, stored(&n, "free").await);
    let held = ctx.state.lock(&deferred.call_ref).await;

    send(&n, ReplCommand::ReclaimAll);
    send(&n, ReplCommand::ReclaimAll);
    sip_clock::testkit::settle().await;
    assert!(ctx.state.is_parked(&free.call_ref), "the free call is parked at once");
    n.membership.set_conditions("w0", true, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(armed(&n, &free.call_ref), "armed once the endpoint reads ready");
    assert!(!ctx.state.contains(&deferred.call_ref), "the held call waits for its park");

    drop(held);
    sip_clock::testkit::settle().await;
    assert!(armed(&n, &deferred.call_ref), "the deferred call is armed behind its park");
    hang_up(&n, caller.as_ref(), &free).await;
    hang_up(&n, caller.as_ref(), &deferred).await;
    nothing_left(&n);
}

#[tokio::test(start_paused = true)]
async fn a_request_naming_no_call_ref_of_a_deferred_call_is_served_by_the_reclaimed_call() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w0", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let call = stored(&n, "no-call-ref").await;
    let held = ctx.state.lock(&call.call_ref).await;

    send(&n, ReplCommand::ReclaimAll);
    sip_clock::testkit::settle().await;
    caller.send_to(&caller_bye(&call), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let lines = heard(caller.as_ref(), Duration::from_millis(100)).await;
    assert!(lines.is_empty(), "the BYE waits behind the call's park: {lines:?}");

    drop(held);
    let lines = heard(caller.as_ref(), Duration::from_millis(500)).await;
    assert_eq!(lines, ["SIP/2.0 200 OK"], "the reclaimed call takes the BYE");
    let cdrs: Vec<String> = n.cdr.snapshot().into_iter().map(|c| c.call_ref).collect();
    assert_eq!(cdrs, [call.call_ref.clone()], "the call's one CDR");
    nothing_left(&n);
}

/// The responses `peer` receives within `within`, whole.
async fn received(peer: &dyn UdpEndpoint, within: Duration) -> Vec<String> {
    let mut texts = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some(p)) = tokio::time::timeout_at(deadline, peer.recv()).await {
        texts.push(String::from_utf8_lossy(&p.raw).to_string());
    }
    texts
}

/// The caller's ACK of the non-2xx final `response` to `cid`'s INVITE: the
/// INVITE's branch, the final's To (RFC 3261 §17.1.1.3).
fn ack_of(cid: &str, response: &str) -> Vec<u8> {
    let to = response.lines().find(|l| l.starts_with("To:")).expect("the final's To");
    format!(
        "ACK sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         {to}\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_retransmission_of_a_deferred_calls_invite_is_served_by_the_reclaimed_call() {
    retransmitted_to_a_deferred_call(false).await;
}

/// Behind a closed gate the retransmission waits for the arming, which seeds
/// the INVITE's transaction and asks the undecided call's decision again.
#[tokio::test(start_paused = true)]
async fn a_retransmission_behind_a_closed_gate_waits_for_the_deferred_calls_arming() {
    retransmitted_to_a_deferred_call(true).await;
}

/// The caller retransmits the INVITE of an undecided call of the node's own
/// while its park is deferred, the node's endpoint reading ready or, when
/// `closed`, not; the reclaimed call answers it.
async fn retransmitted_to_a_deferred_call(closed: bool) {
    let net = SimulatedSignalingNetwork::new(1);
    let busy = ScriptedDecisionEngine::builder().fallback(|_| reject(486, "Busy Here")).build();
    let n = node_deciding(&net, "w0", Arc::new(busy), |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    if closed {
        n.membership.set_conditions("w0", false, false);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The caller's INVITE reached the node before it went down, undecided.
    let cid = "retransmitted";
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    let call = build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut ids, 0);
    put_body(&n.store, PRI, "w0", &call).await;
    let held = ctx.state.lock(&call.call_ref).await;

    send(&n, ReplCommand::ReclaimAll);
    sip_clock::testkit::settle().await;
    let again = invite("w0", "w1", cid);
    caller.send_to(again.image(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(ctx.unborn.count(), 0, "the retransmission is admitted as no new call");
    drop(held);
    if closed {
        let lines = heard(caller.as_ref(), Duration::from_millis(200)).await;
        assert!(lines.is_empty(), "nothing answers before the arming: {lines:?}");
        assert!(ctx.state.is_parked(&call.call_ref), "parked, not armed");
        n.membership.set_conditions("w0", true, false);
    }

    // The reclaimed call's decision answers the caller on the INVITE's
    // transaction, which answers the retransmission with that final again
    // (RFC 3261 §17.2.1).
    let responses = received(caller.as_ref(), Duration::from_millis(500)).await;
    let finals: Vec<&String> = responses.iter().filter(|r| !r.starts_with("SIP/2.0 1")).collect();
    assert!(!finals.is_empty(), "a final reaches the caller: {responses:?}");
    assert!(finals.iter().all(|f| *f == finals[0]), "one final, repeated: {finals:?}");
    assert!(finals[0].starts_with("SIP/2.0 486"), "the reclaimed call's decision: {finals:?}");
    caller.send_to(&ack_of(cid, finals[0]), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    sip_clock::testkit::settle().await;
    let counts = crate::new_calls::NewCallCounts::compose(
        ctx.metrics.new_calls(),
        Default::default(),
        Default::default(),
    );
    assert_eq!(counts.total(), 0, "the retransmission is no new call");
    let cdrs: Vec<String> = n.cdr.snapshot().into_iter().map(|c| c.call_ref).collect();
    assert_eq!(cdrs, [call.call_ref.clone()], "the reclaimed call's one CDR");
    tokio::time::sleep(Duration::from_secs(40)).await;
    nothing_left(&n);
}
