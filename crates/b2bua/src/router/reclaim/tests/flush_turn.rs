//! A replicated flush crossing a call's turn. The turn holds the call's lock,
//! as a consult holds it across its decision round trip: the router loop queues
//! the flush behind it and goes on answering the proxy's probes, and the flush
//! runs once the lock is free. A flush that finds no call leaves nothing behind.

use std::net::SocketAddr;
use std::time::Duration;

use call::CallModelState;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use super::{answered, body, live_versions, stamped, unanswered};
use crate::router::test_support::{node_on, Node, NODE_SIP_ADDR};
use crate::router::ReplCommand;
use crate::store::{CallStore, MaterialiseOrigin, PartitionRole, PutOpts};

const PROBER: &str = "10.0.0.1:5060";
/// How long the proxy's probe waits at most, well under its reply window.
const ANSWERED_WITHIN: Duration = Duration::from_millis(50);

/// `ordinal`'s node and a prober on its SIP fabric.
async fn node_and_prober(ordinal: &str) -> (Node, Box<dyn UdpEndpoint>) {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(&net, ordinal, |_| {}, Vec::new()).await;
    let prober = net.bind_udp(BindUdpOpts::new(PROBER.parse().unwrap(), 64)).await.unwrap();
    (node, prober)
}

fn send(n: &Node, cmd: ReplCommand) {
    n.core.fail_back_sender().expect("a wired node").send(cmd).unwrap();
}

/// A probe's out-of-dialog OPTIONS draws its answer within [`ANSWERED_WITHIN`].
async fn assert_answers_a_probe(prober: &dyn UdpEndpoint, branch: &str) {
    let options = format!(
        "OPTIONS sip:worker@127.0.0.2:5080 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {PROBER};branch=z9hG4bK-{branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:probe@10.0.0.1>;tag=probe\r\n\
         To: <sip:worker@127.0.0.2>\r\n\
         Call-ID: {branch}@probe\r\n\
         CSeq: 1 OPTIONS\r\n\
         Content-Length: 0\r\n\r\n"
    );
    prober.send_to(options.as_bytes(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    tokio::time::sleep(ANSWERED_WITHIN).await;
    assert!(prober.try_recv().is_some(), "the router answers its probe while the flush waits");
}

/// A refused reverse flush crossing the call's turn: OPTIONS stay answered, and
/// the copy meets the live call once the turn lets go of the lock.
#[tokio::test(start_paused = true)]
async fn a_refused_reverse_flush_crossing_a_turn_leaves_options_answered() {
    let (n, prober) = node_and_prober("w0").await;
    let ctx = n.core.router_ctx();
    let live = answered(6, 1, CallModelState::Active);
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));
    let held = ctx.state.lock(&call_ref).await;

    let copy = stamped(answered(4, 2, CallModelState::Active), 4, 1);
    let cmd = ReplCommand::ReverseFlushRefused {
        call_ref: call_ref.clone(),
        body: body(&copy).into(),
        origin_now_ms: 0,
    };
    send(&n, cmd);
    assert_answers_a_probe(&*prober, "reverse").await;
    assert_eq!(live_versions(&n, &call_ref).0, (6, 1), "the turn holding the lock runs first");

    drop(held);
    sip_clock::testkit::settle().await;
    assert_eq!(live_versions(&n, &call_ref).0, (6, 2), "the copy met the live call");
}

/// A refused forward flush crossing a takeover copy's turn, as a copy's re-asked
/// consult holds it: OPTIONS stay answered, and the primary's copy meets it.
#[tokio::test(start_paused = true)]
async fn a_refused_forward_flush_crossing_a_turn_leaves_options_answered() {
    let (n, prober) = node_and_prober("w1").await;
    let ctx = n.core.router_ctx();
    let copy = unanswered(4, 1, CallModelState::Active);
    let call_ref = copy.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(copy, MaterialiseOrigin::Takeover));
    let held = ctx.state.lock(&call_ref).await;

    let primary = answered(5, 1, CallModelState::Active);
    let cmd = ReplCommand::ForwardFlushRefused {
        call_ref: call_ref.clone(),
        body: body(&primary).into(),
        origin_now_ms: 0,
    };
    send(&n, cmd);
    assert_answers_a_probe(&*prober, "forward").await;
    assert!(ctx.state.peek(&call_ref).is_some(), "the turn holding the lock runs first");

    drop(held);
    sip_clock::testkit::settle().await;
    assert_eq!(
        n.metrics.repl_stand_down_total(),
        1,
        "the primary's copy covers it: it stands down"
    );
    assert!(ctx.state.peek(&call_ref).is_none());
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}

/// A reverse flush the store took, reaching a live call under a turn: OPTIONS
/// stay answered, and the reconcile runs once the lock is free.
#[tokio::test(start_paused = true)]
async fn a_reconcile_crossing_a_turn_leaves_options_answered() {
    let (n, prober) = node_and_prober("w0").await;
    let ctx = n.core.router_ctx();
    let live = unanswered(4, 1, CallModelState::Active);
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));
    let stored = body(&answered(4, 2, CallModelState::Active));
    let (pri, opts) = (PartitionRole::Primary, PutOpts::default());
    n.store.put_call(pri, "w0", &call_ref, stored, &[], 60_000, 4, 2, &opts).await.unwrap();
    let held = ctx.state.lock(&call_ref).await;

    send(&n, ReplCommand::ReclaimCall(call_ref.clone()));
    assert_answers_a_probe(&*prober, "reconcile").await;
    assert_eq!(live_versions(&n, &call_ref).0, (4, 1), "the turn holding the lock runs first");

    drop(held);
    sip_clock::testkit::settle().await;
    assert_eq!(live_versions(&n, &call_ref).0, (4, 2), "the backup's copy is taken");
}

/// A straggler's reclaim and a refused flush behind it: the flush finds no live
/// copy when it is offered, but the reclaim's turn is queued ahead of it, and it
/// meets the copy that turn makes live.
#[tokio::test(start_paused = true)]
async fn a_refused_flush_behind_a_reclaim_meets_the_copy_the_reclaim_makes_live() {
    let (n, _prober) = node_and_prober("w0").await;
    let ctx = n.core.router_ctx();
    let stored = answered(6, 1, CallModelState::Active);
    let call_ref = stored.call.call_ref.clone();
    let (pri, opts) = (PartitionRole::Primary, PutOpts::default());
    n.store.put_call(pri, "w0", &call_ref, body(&stored), &[], 60_000, 6, 1, &opts).await.unwrap();
    let held = ctx.state.lock(&call_ref).await;

    send(&n, ReplCommand::ReclaimCall(call_ref.clone()));
    let copy = stamped(answered(4, 2, CallModelState::Active), 4, 1);
    let cmd = ReplCommand::ReverseFlushRefused {
        call_ref: call_ref.clone(),
        body: body(&copy).into(),
        origin_now_ms: 0,
    };
    send(&n, cmd);
    sip_clock::testkit::settle().await;
    assert!(ctx.state.peek(&call_ref).is_none(), "the reclaim waits for the lock");

    drop(held);
    sip_clock::testkit::settle().await;
    assert_eq!(live_versions(&n, &call_ref).0, (7, 2), "the reclaim's write, and the copy met it");
}

/// A deferred terminal reaching a call not live here is discharged on its turn,
/// and the turn's queue and lock entry go with it.
#[tokio::test(start_paused = true)]
async fn a_reconcile_of_a_deferred_terminal_discharges_it_and_leaves_nothing_behind() {
    let (n, _prober) = node_and_prober("w0").await;
    let ctx = n.core.router_ctx();
    let terminal = answered(2, 1, CallModelState::Terminated);
    let call_ref = terminal.call.call_ref.clone();
    let (pri, opts) = (PartitionRole::Primary, PutOpts::default());
    let stored = body(&terminal);
    n.store.put_call(pri, "w0", &call_ref, stored, &[], 60_000, 2, 1, &opts).await.unwrap();

    send(&n, ReplCommand::ReclaimCall(call_ref.clone()));
    sip_clock::testkit::settle().await;
    assert_eq!(n.cdr.snapshot().len(), 1, "the deferred end's one CDR");
    assert!(ctx.state.peek(&call_ref).is_none());
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
    assert_eq!(ctx.dispatcher.queue_count(), 0, "no queue is left behind");
}

/// A reconcile of a body the store no longer holds, and a refused flush of an
/// Element with no copy here: nothing is held, nothing opened, nothing left.
#[tokio::test(start_paused = true)]
async fn a_flush_that_finds_no_call_leaves_nothing_behind() {
    let (n, _prober) = node_and_prober("w0").await;
    let ctx = n.core.router_ctx();
    let gone = answered(2, 1, CallModelState::Active);
    let call_ref = gone.call.call_ref.clone();

    send(&n, ReplCommand::ReclaimCall(call_ref.clone()));
    sip_clock::testkit::settle().await;
    assert_eq!(ctx.state.lock_count(), 0, "the reconcile's lock entry is released");
    assert_eq!(ctx.dispatcher.queue_count(), 0, "the reconcile's queue is released");

    let cmd = ReplCommand::ReverseFlushRefused {
        call_ref: call_ref.clone(),
        body: body(&gone).into(),
        origin_now_ms: 0,
    };
    send(&n, cmd);
    sip_clock::testkit::settle().await;
    assert_eq!(ctx.state.lock_count(), 0, "an Element alone takes no lock");
    assert_eq!(ctx.dispatcher.queue_count(), 0, "and opens no queue");
}
