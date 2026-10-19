//! The departure sweep over a wired core: of a departed peer's backup
//! partition it takes over exactly the calls with an INVITE transaction to
//! serve and discharges the ended ones, and an
//! undecided one among them is answered 503 through the caller's INVITE
//! transaction, or settled with nothing on the wire when that transaction's
//! branch is already another call's here.

use std::net::SocketAddr;
use std::time::Duration;

use call::{Call, CallBodyCodec, CallModelState, LegState, MsgpackCodec};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use b2bua_sdk::event::CallEvent;

use super::{sweep, take_over_departed, take_over_one, Swept};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::repl::ReplicatingCallStore;
use crate::router::test_support::{invite, node_on, src, NODE_SIP_ADDR};
use crate::store::{CallStore, PartitionRole, PutOpts};

const BAK: PartitionRole = PartitionRole::Backup;

/// A call `w0` owns and `w1` backs up, as its INVITE's turn created it.
fn born(cid: &str) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(&invite("w0", "w1", cid), src(), &config, &sip_txn::IdGen::seeded(1), 0)
}

/// A call whose caller was answered and that has nothing in flight.
fn quiet(cid: &str) -> Call {
    let mut call = born(cid);
    call.a_leg.invite_final_sent = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    call
}

/// A call that already ended.
fn ended(cid: &str) -> Call {
    let mut call = born(cid);
    call.a_leg.invite_final_sent = Some(486);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Terminated);
    call.state = CallModelState::Terminated;
    call::helpers::record_termination(call, 0, call::TerminationCause::RemoteFinal, None)
}

async fn put(store: &ReplicatingCallStore, call: &Call) {
    let body = MsgpackCodec::new().encode(call).unwrap();
    store
        .put_call(BAK, "w0", &call.call_ref, body, &[], 60_000, 1, 0, &PutOpts::default())
        .await
        .unwrap();
}

/// The `w0` backup body of `call_ref` this node holds, decoded.
async fn element(store: &ReplicatingCallStore, call_ref: &str) -> Call {
    let body = store.get_call(BAK, "w0", call_ref).await.unwrap().expect("the element");
    MsgpackCodec::new().decode(&body).unwrap()
}

/// The status lines `caller` receives within `within`.
async fn heard(caller: &dyn UdpEndpoint, within: Duration) -> Vec<String> {
    let mut lines = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some(p)) = tokio::time::timeout_at(deadline, caller.recv()).await {
        let text = String::from_utf8_lossy(&p.raw).to_string();
        lines.push(text.lines().next().unwrap_or_default().to_string());
    }
    lines
}

#[tokio::test(start_paused = true)]
async fn a_departed_peers_in_flight_calls_are_taken_over_and_its_ended_calls_discharged() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let (undecided, quiet, ended) = (born("undecided"), quiet("quiet"), ended("ended"));
    for call in [&undecided, &quiet, &ended] {
        put(&n.store, call).await;
    }

    take_over_departed(ctx, "w0");

    let lines = heard(caller.as_ref(), Duration::from_millis(200)).await;
    assert_eq!(lines, ["SIP/2.0 503 Service Unavailable"], "the undecided caller's final");
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 1, "one call taken over");
    assert_eq!(n.metrics.repl_takeover_refused_terminated_total(), 0, "an ended call unread");
    assert!(ctx.state.peek(&quiet.call_ref).is_none(), "the quiet call stays reactive");
    assert!(ctx.state.peek(&undecided.call_ref).is_none(), "the settled copy released itself");
    assert_eq!(
        element(&n.store, &undecided.call_ref).await.state,
        CallModelState::Terminated,
        "the settled call's end is kept for its primary",
    );
    let cdrs: Vec<String> = n.cdr.snapshot().into_iter().map(|r| r.call_ref).collect();
    assert_eq!(
        cdrs,
        [ended.call_ref.clone()],
        "the ended call is discharged; the settled copy defers"
    );
    let held = n.store.get_call(BAK, "w0", &ended.call_ref).await.unwrap();
    assert!(held.is_none(), "the discharged call is deleted");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}

/// A call its copy ends while its primary is out of the membership set has no
/// primary to discharge it: the copy writes the one CDR and deletes the call.
#[tokio::test(start_paused = true)]
async fn a_call_ended_while_its_primary_is_departed_is_discharged_by_its_copy() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let undecided = born("departed-primary");
    put(&n.store, &undecided).await;
    n.membership.add(topology::Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    n.membership.remove("w0");

    let lines = heard(caller.as_ref(), Duration::from_millis(500)).await;
    assert_eq!(lines, ["SIP/2.0 503 Service Unavailable"], "the undecided caller's final");
    let cdrs = n.cdr.snapshot();
    assert_eq!(cdrs.len(), 1, "the copy writes the call's CDR");
    assert_eq!(cdrs[0].call_ref, undecided.call_ref);
    let held = n.store.get_call(BAK, "w0", &undecided.call_ref).await.unwrap();
    assert!(held.is_none(), "the ended call is deleted");
    assert!(ctx.state.peek(&undecided.call_ref).is_none(), "no live copy is left");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}

#[tokio::test(start_paused = true)]
async fn an_undecided_call_whose_branch_is_another_calls_here_is_settled_off_the_wire() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |c| c.overload_panic_elu_threshold = 1.1, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let undecided = born("replaced");
    put(&n.store, &undecided).await;
    // The caller's retransmission, placed on this node as a call of its own.
    let copy = invite("w1", "w0", "replaced");
    caller.send_to(copy.image(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let lines = heard(caller.as_ref(), Duration::from_millis(100)).await;
    assert_eq!(lines, ["SIP/2.0 100 Trying"], "the copy is served here");

    take_over_departed(ctx, "w0");

    let lines = heard(caller.as_ref(), Duration::from_millis(200)).await;
    assert!(lines.is_empty(), "nothing reaches the caller on the other call's transaction");
    assert_eq!(
        element(&n.store, &undecided.call_ref).await.state,
        CallModelState::Terminated,
        "the undecided call is settled all the same",
    );
}

/// The sweep runs off the router loop: while a departed peer's partition of
/// quiet calls and undecided ones is swept, the router keeps serving its own
/// events — a new call of this node's own is born before the sweep is done.
#[tokio::test(start_paused = true)]
async fn the_router_serves_its_own_calls_while_a_departed_partition_is_swept() {
    const QUIET: usize = 2_000;
    const UNDECIDED: usize = 200;
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |c| c.overload_panic_elu_threshold = 1.1, Vec::new()).await;
    let _caller = net.bind_udp(BindUdpOpts::new(src(), 4096)).await.unwrap();
    let ctx = n.core.router_ctx();
    for i in 0..QUIET {
        put(&n.store, &quiet(&format!("quiet-{i}"))).await;
    }
    for i in 0..UNDECIDED {
        put(&n.store, &born(&format!("undecided-{i}"))).await;
    }
    n.membership.add(topology::Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    n.membership.remove("w0");
    while n.metrics.repl_takeover_hydrated_total() == 0 {
        tokio::task::yield_now().await;
    }

    // A new call of this node's own reaches the router mid-sweep.
    let config = B2buaConfig { self_ordinal: "w1".into(), ..Default::default() };
    let own = invite("w1", "w0", "own");
    let own_ref = build_initial_call(&own, src(), &config, &sip_txn::IdGen::seeded(1), 0).call_ref;
    let event = CallEvent::Sip {
        message: Box::new(sip_message::SipMessage::Request(own)),
        src: src(),
        matched_client_txn: false,
    };
    ctx.reentry_tx.send(event).unwrap();
    let mut rounds = 0;
    while ctx.state.peek(&own_ref).is_none() {
        rounds += 1;
        assert!(rounds < 100_000, "the new call is never born");
        tokio::task::yield_now().await;
    }
    let taken = n.metrics.repl_takeover_hydrated_total();
    assert!(
        taken < UNDECIDED as u64,
        "the new call is born while the sweep is under way ({taken} of {UNDECIDED} taken)",
    );
}

/// A transaction layer that cannot be asked seeds nothing, and that is no
/// branch held by another call: the undecided call is not ended off the wire.
#[tokio::test(start_paused = true)]
async fn an_undecided_call_whose_seeding_failed_is_not_settled() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let ctx = n.core.router_ctx();
    let undecided = born("unseeded");
    put(&n.store, &undecided).await;
    ctx.txn.abort_owner();
    tokio::time::sleep(Duration::from_millis(10)).await;

    take_over_departed(ctx, "w0");
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 1, "the call was taken over");
    assert_eq!(
        element(&n.store, &undecided.call_ref).await.state,
        CallModelState::Active,
        "a seeding that failed settles nothing",
    );
}

/// A crash stops a sweep under way: no call is taken over after the core is
/// aborted.
#[tokio::test(start_paused = true)]
async fn an_aborted_core_takes_nothing_more_over() {
    const UNDECIDED: usize = 300;
    let net = SimulatedSignalingNetwork::new(1);
    let mut n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let _caller = net.bind_udp(BindUdpOpts::new(src(), 4096)).await.unwrap();
    for i in 0..UNDECIDED {
        put(&n.store, &born(&format!("aborted-{i}"))).await;
    }
    take_over_departed(n.core.router_ctx(), "w0");
    while n.metrics.repl_takeover_hydrated_total() == 0 {
        tokio::task::yield_now().await;
    }
    n.core.abort();
    let at_abort = n.metrics.repl_takeover_hydrated_total();
    for _ in 0..10_000 {
        tokio::task::yield_now().await;
    }
    assert!(at_abort < UNDECIDED as u64, "the sweep was under way at the abort");
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), at_abort, "nothing taken over after it");
}

/// A call whose takeover panics costs that call only: the sweep takes the
/// others over and counts the panic.
#[tokio::test(start_paused = true)]
async fn a_takeover_that_panics_costs_its_call_only_and_is_counted() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    let (first, bad, last) = (born("first"), born("bad"), born("last"));
    for call in [&first, &bad, &last] {
        put(&n.store, call).await;
    }
    let refs = vec![first.call_ref.clone(), bad.call_ref.clone(), last.call_ref.clone()];
    let bad_ref = bad.call_ref.clone();

    let task = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            sweep(&ctx, "w0", refs, move |ctx, call_ref| {
                let bad = call_ref == bad_ref;
                async move {
                    assert!(!bad, "the takeover of this call panics");
                    take_over_one(&ctx, &call_ref).await
                }
            })
            .await;
        }
    });

    let lines = heard(caller.as_ref(), Duration::from_millis(200)).await;
    let answered = ["SIP/2.0 503 Service Unavailable", "SIP/2.0 503 Service Unavailable"];
    assert_eq!(lines, answered, "both other undecided callers are answered");
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 2, "the other two calls taken over");
    assert_eq!(n.metrics.departure_takeover_panics_total(), 1, "the panic is counted");
    assert!(task.await.is_ok(), "the sweep itself ran to its end");
    let _ = Swept::Left;
}
