use std::time::Duration;

use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;

use super::*;
use crate::admission::Class;
use crate::new_calls::NewCallCounts;
use crate::router::test_support::{node_on, NODE_SIP_ADDR};

const PEER: &str = "10.0.0.1:5060";

/// A new INVITE on one identity (Call-ID, From-tag), on Via branch
/// `branch`.
fn invite(branch: &str) -> Vec<u8> {
    invite_cseq(branch, 1)
}

/// [`invite`] with CSeq number `cseq`.
fn invite_cseq(branch: &str, cseq: u32) -> Vec<u8> {
    format!(
        "INVITE sip:bob@127.0.0.2:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP {PEER};branch={branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.2>\r\n\
             Call-ID: copy@unit\r\n\
             CSeq: {cseq} INVITE\r\n\
             Contact: <sip:alice@10.0.0.1:5060>\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A second branch of one INVITE (RFC 3261 §8.2.2.2) reaching a call
/// already here is that call's copy: not judged at admission, it spends
/// no CPS token and is never counted as a new call.
#[tokio::test(start_paused = true)]
async fn a_second_branch_of_an_admitted_invite_is_not_judged_again() {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(
        &net,
        "w0",
        |c| {
            c.cps_bucket_size = 2;
            c.cps_bucket_rate = 0;
            c.overload_panic_elu_threshold = 1.1;
        },
        Vec::new(),
    )
    .await;
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = node.core.router_ctx().clone();
    let to = SocketAddr::from(NODE_SIP_ADDR);
    peer.send_to(&invite("z9hG4bK-first"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ctx.overload.metrics().token_bucket_level, 1.0, "the INVITE spent one");

    peer.send_to(&invite("z9hG4bK-second"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ctx.overload.metrics().token_bucket_level, 1.0, "the copy spends none");
    let counts =
        NewCallCounts::compose(ctx.metrics.new_calls(), Default::default(), Default::default());
    assert_eq!(counts.accepted(Class::Normal), 1);
    assert_eq!(counts.total(), 1, "the copy is no new call");
    assert_eq!(ctx.unborn.count(), 0, "the admitted call is born");
}

/// An INVITE on an admitted call's identity with the next CSeq is a new
/// request (RFC 3261 §8.1.3.5), not a copy: it is judged, and an empty
/// bucket refuses it.
#[tokio::test(start_paused = true)]
async fn the_next_cseq_on_an_admitted_identity_is_judged() {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(
        &net,
        "w0",
        |c| {
            c.cps_bucket_size = 1;
            c.cps_bucket_rate = 0;
            c.overload_panic_elu_threshold = 1.1;
        },
        Vec::new(),
    )
    .await;
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = node.core.router_ctx().clone();
    let call_ref = call::derive_call_ref("w0", "copy@unit", "a");
    // The first INVITE's turn parks on the held lock, its call unborn.
    let held = ctx.state.lock(&call_ref).await;
    let to = SocketAddr::from(NODE_SIP_ADDR);
    peer.send_to(&invite("z9hG4bK-first"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.send_to(&invite_cseq("z9hG4bK-retry", 2), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let counts =
        NewCallCounts::compose(ctx.metrics.new_calls(), Default::default(), Default::default());
    assert_eq!(counts.rejected(crate::new_calls::Refusal::BucketEmpty, Class::Normal), 1);
    assert_eq!(ctx.unborn.count(), 1, "only the first INVITE holds a call");
    drop(held);
}

/// The peer's CANCEL of the INVITE on Via branch `branch` with CSeq
/// `cseq` (RFC 3261 §9.1).
fn cancel_cseq(branch: &str, cseq: u32) -> Vec<u8> {
    format!(
        "CANCEL sip:bob@127.0.0.2:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP {PEER};branch={branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.2>\r\n\
             Call-ID: copy@unit\r\n\
             CSeq: {cseq} CANCEL\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A new request on a live call's identity, CANCELed while its turn
/// waits, ends at the identity guard: its setup-CANCEL mark goes with
/// it, and the live call holds no mark it never reads.
#[tokio::test(start_paused = true)]
async fn a_cancelled_new_request_on_a_live_identity_leaves_no_mark() {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(&net, "w0", |c| c.overload_panic_elu_threshold = 1.1, Vec::new()).await;
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = node.core.router_ctx().clone();
    let call_ref = call::derive_call_ref("w0", "copy@unit", "a");
    let to = SocketAddr::from(NODE_SIP_ADDR);
    peer.send_to(&invite("z9hG4bK-live"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(ctx.state.peek(&call_ref).is_some(), "the first INVITE's call is live");

    // The new request's turn parks on the held lock; its caller CANCELs.
    let held = ctx.state.lock(&call_ref).await;
    peer.send_to(&invite_cseq("z9hG4bK-new", 2), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.send_to(&cancel_cseq("z9hG4bK-new", 2), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ctx.state.setup_cancelled_count(), 1, "the CANCEL marked the new request");
    drop(held);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let counts =
        NewCallCounts::compose(ctx.metrics.new_calls(), Default::default(), Default::default());
    assert_eq!(
        counts.rejected(crate::new_calls::Refusal::IdentityInUse, Class::Normal),
        1,
        "the new request ended at the identity guard"
    );
    assert_eq!(ctx.state.setup_cancelled_count(), 0, "its mark went with it");
    assert!(ctx.state.peek(&call_ref).is_some(), "the live call lives on");
}

/// Each hold counts one call until it is dropped.
#[test]
fn an_unborn_call_counts_until_dropped() {
    let unborn = Unborn::default();
    let first = unborn.admit("c", 1);
    let second = unborn.clone().admit("c", 1);
    let other = unborn.admit("d", 1);
    let retry = unborn.admit("c", 2);
    assert_eq!(unborn.count(), 4);
    assert!(unborn.holds("c", 1) && unborn.holds("d", 1) && unborn.holds("c", 2));
    assert!(!unborn.holds("d", 2), "a hold is keyed by its INVITE's CSeq");
    drop(first);
    assert!(unborn.holds("c", 1), "a second hold on the call remains");
    drop(second);
    assert!(!unborn.holds("c", 1));
    assert!(unborn.holds("c", 2), "the retry's hold is its own");
    drop((other, retry));
    assert_eq!(unborn.count(), 0);
    assert!(!unborn.holds("d", 1));
    assert!(unborn.inner.lock().is_empty(), "no call keeps an entry once its holds drop");
}

/// A peer's in-dialog BYE naming the `callRef` of identity (`copy@unit`,
/// `a`) in its Request-URI, a call this node never admitted.
fn bye_naming(call_ref: &str) -> Vec<u8> {
    format!(
        "BYE sip:b2bua@127.0.0.2:5080;callRef={} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {PEER};branch=z9hG4bK-bye\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.2>;tag=b\r\n\
             Call-ID: copy@unit\r\n\
             CSeq: 2 BYE\r\n\
             Content-Length: 0\r\n\r\n",
        crate::stack_identity::encode_param(call_ref)
    )
    .into_bytes()
}

/// A queue opened by another request on a call's `callRef` is no
/// admitted call: a new INVITE on that identity is judged all the same,
/// and an empty bucket refuses it.
#[tokio::test(start_paused = true)]
async fn a_queue_opened_by_another_request_does_not_admit_a_new_invite() {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(
        &net,
        "w0",
        |c| {
            c.cps_bucket_size = 1;
            c.cps_bucket_rate = 0;
            c.overload_panic_elu_threshold = 1.1;
        },
        Vec::new(),
    )
    .await;
    node.core.overload().spend_token();
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = node.core.router_ctx().clone();
    let call_ref = call::derive_call_ref("w0", "copy@unit", "a");
    // The BYE's turn opens the call's queue and parks on the held lock.
    let held = ctx.state.lock(&call_ref).await;
    let to = SocketAddr::from(NODE_SIP_ADDR);
    peer.send_to(&bye_naming(&call_ref), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(ctx.dispatcher.has_queue(&call_ref), "the BYE opened the call's queue");

    peer.send_to(&invite("z9hG4bK-new"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let counts =
        NewCallCounts::compose(ctx.metrics.new_calls(), Default::default(), Default::default());
    assert_eq!(counts.rejected(crate::new_calls::Refusal::BucketEmpty, Class::Normal), 1);
    assert_eq!(counts.accepted(Class::Normal), 0);
    drop(held);
}

/// A new INVITE on a `callRef` whose queue holds its previous call's
/// release, a request still ahead of it, is judged and waits for the
/// release: its call is born on the identity's next queue, never refused
/// for arriving behind the release.
#[tokio::test(start_paused = true)]
async fn a_new_invite_behind_its_identitys_queued_release_is_born_after_it() {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(&net, "w0", |c| c.overload_panic_elu_threshold = 1.1, Vec::new()).await;
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = node.core.router_ctx().clone();
    let call_ref = call::derive_call_ref("w0", "copy@unit", "a");
    // The BYE's turn opens the queue and parks on the held lock; the
    // previous call's release queues behind it.
    let held = ctx.state.lock(&call_ref).await;
    let to = SocketAddr::from(NODE_SIP_ADDR);
    peer.send_to(&bye_naming(&call_ref), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    ctx.dispatcher.release(&call_ref, crate::metrics::RemovalClass::Terminated);

    peer.send_to(&invite("z9hG4bK-new"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let counts =
        NewCallCounts::compose(ctx.metrics.new_calls(), Default::default(), Default::default());
    assert_eq!(counts.rejected(crate::new_calls::Refusal::DispatchDiscard, Class::Normal), 0);
    assert_eq!(counts.accepted(Class::Normal), 1, "the new call is born after the release");
    assert!(ctx.state.peek(&call_ref).is_some());
    assert!(ctx.dispatcher.has_queue(&call_ref), "it runs on the identity's next queue");
}

/// A copy of an admitted INVITE whose turn ends with the store-fault 500
/// is answered by that fault too, and counted as a copy: the INVITE is
/// counted once.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_store_faulted_invite_is_counted_as_a_copy() {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(&net, "w0", |c| c.overload_panic_elu_threshold = 1.1, Vec::new()).await;
    let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
    let ctx = node.core.router_ctx().clone();
    let call_ref = call::derive_call_ref("w0", "copy@unit", "a");
    // The INVITE's turn parks on the held lock, its call unborn.
    let held = ctx.state.lock(&call_ref).await;
    let to = SocketAddr::from(NODE_SIP_ADDR);
    peer.send_to(&invite("z9hG4bK-first"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.send_to(&invite("z9hG4bK-second"), to).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ctx.unborn.count(), 1, "the copy holds nothing");

    ctx.store_faults.arm(crate::store::StoreFaultPoint::LiveInitialInvite);
    drop(held);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let counts =
        NewCallCounts::compose(ctx.metrics.new_calls(), Default::default(), Default::default());
    assert_eq!(counts.rejected(crate::new_calls::Refusal::StoreFault, Class::Normal), 1);
    assert_eq!(counts.total(), 1, "the INVITE is counted once");
    assert_eq!(counts.refused_copies(), 1);
    assert_eq!(ctx.state.active_count(), 0);
}
