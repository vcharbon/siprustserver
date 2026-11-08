//! The departure sweep crossing a release of the same call: a takeover copy
//! shedding at quiescence is still tearing its timers and transactions down
//! when its primary, which wrote the call again, departs, and the sweep takes
//! the call over from the Element that write left. The sweep's takeover runs
//! after the release, under the call's one lock, so the release tears down
//! nothing of the copy it makes: that copy sends its awaited request again,
//! is watched, and sheds once the answer leaves nothing in flight.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use call::{Call, MsgpackCodec, OffTurnRequest, TimerKey, TimerType};
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use tokio::sync::watch;

use crate::decision::ScriptedDecisionEngine;
use crate::router::materialise::{materialise, Materialised, Origin};
use crate::router::test_support::{
    answered, caller_bye, heard, node_probed, put_body, src, NODE_SIP_ADDR,
};
use crate::store::{envelope, CallStore, PartitionRole};
use crate::turn_probe::{Hold, TurnMark, TurnProbe, TurnProbes};

const BAK: PartitionRole = PartitionRole::Backup;

/// Holds the release of one call once armed, until the test lets it go.
struct HoldRelease {
    call_ref: String,
    armed: AtomicBool,
    go: watch::Receiver<bool>,
}

impl TurnProbe for HoldRelease {
    fn reached(&self, _mark: &TurnMark<'_>) -> Option<Hold> {
        None
    }

    fn release_step(&self, _node: &str, call_ref: &str) -> Option<Hold> {
        if call_ref != self.call_ref || !self.armed.swap(false, Ordering::SeqCst) {
            return None;
        }
        let mut go = self.go.clone();
        Some(Box::pin(async move {
            let _ = go.wait_for(|go| *go).await;
        }))
    }
}

/// An answered call `w0` owns and `w1` backs up, awaiting a service's HTTP
/// answer, its duration capped an hour out.
fn awaiting(cid: &str) -> Call {
    let mut call = answered("w0", "w1", cid);
    call.awaited.push(OffTurnRequest::ServiceHttp {
        correlation_id: "svc:ask".into(),
        endpoint: "/ask".into(),
        method: "POST".into(),
        headers: vec![],
        body: b"q".to_vec(),
        content_type: None,
        timeout_ms: None,
    });
    call::helpers::arm_timer(&mut call, TimerKey::call(TimerType::GlobalDuration), 3_600_000);
    call
}

#[tokio::test(start_paused = true)]
async fn a_takeover_crossing_a_release_runs_after_it_with_its_watch_and_its_resend() {
    let (_capture, log) = observe_testkit::test_buffer();
    let net = SimulatedSignalingNetwork::new(1);
    let call = awaiting("crossing");
    let r = call.call_ref.clone();
    let (go, held) = watch::channel(false);
    let hold =
        Arc::new(HoldRelease { call_ref: r.clone(), armed: AtomicBool::new(false), go: held });
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.2", 9));
    let probes = TurnProbes::new(hold.clone());
    let n = node_probed(&net, "w1", decision, |_| {}, Vec::new(), probes).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let ctx = n.core.router_ctx();
    n.membership.add(topology::Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The copy serves the call, sends its awaited request again and takes its
    // answer; with nothing left in flight it sheds, its release held once it
    // left the live map.
    put_body(&n.store, BAK, "w0", &call).await;
    hold.armed.store(true, Ordering::SeqCst);
    {
        let _guard = ctx.state.lock(&r).await;
        let served = materialise(ctx, &r, Origin::Takeover).await;
        assert!(matches!(served, Materialised::Served { .. }), "taken over: {served:?}");
    }
    sip_clock::testkit::settle().await;
    assert_eq!(log.matching("awaited requests sent again").len(), 1);
    assert!(ctx.state.peek(&r).is_none(), "the quiesced copy left the live map");
    assert!(!hold.armed.load(Ordering::SeqCst), "its release is held");

    // The primary wrote the call again, a request of its own awaited, and the
    // backup applied it to the Element; then the primary departs and the
    // sweep takes the call over from that Element.
    put_body(&n.store, BAK, "w0", &call).await;
    n.membership.remove("w0");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!ctx.state.contains(&r), "the sweep waits for the release on the call's lock");
    go.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The sweep's copy sent the request again and took its answer: watched
    // again, it shed once nothing was left in flight, its timers with it.
    assert_eq!(log.matching("awaited requests sent again").len(), 2, "its request went again");
    assert!(ctx.state.peek(&r).is_none(), "the copy shed once its request was answered");
    assert_eq!(n.metrics.timer_live(), 0, "its timers went with it");
    let element = n.store.get_call(BAK, "w0", &r).await.unwrap().expect("the Element");
    let element = envelope::decode(&MsgpackCodec::new(), &element).unwrap().call;
    assert!(element.awaited.is_empty(), "the copy wrote the answer it took");

    // The caller ends the call, taken over on her BYE: one CDR, nothing left
    // behind.
    caller.send_to(&caller_bye(&call), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let lines = heard(caller.as_ref(), Duration::from_millis(500)).await;
    assert_eq!(lines, ["SIP/2.0 200 OK"], "the BYE is answered");
    let cdrs: Vec<String> = n.cdr.snapshot().into_iter().map(|c| c.call_ref).collect();
    assert_eq!(cdrs, std::slice::from_ref(&r), "the copy writes the call's one CDR");
    assert!(ctx.state.peek(&r).is_none(), "no live copy is left");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
    assert!(!ctx.dispatcher.has_queue(&r), "no queue is left behind");
    assert_eq!(n.metrics.timer_live(), 0, "no timer is left behind");
}
