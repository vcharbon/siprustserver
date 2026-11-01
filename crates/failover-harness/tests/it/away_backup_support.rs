//! What the away-backup cells share: a two-worker cluster behind the front
//! proxy, the restart of a worker in its two shapes (the process restarts in
//! place behind a not-ready endpoint; the pod is re-created at a new address),
//! its return, an answered call read back with the cookie the proxy signed,
//! and the graceful drain of a worker driven to its exit.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    worker_ordinals, DrainBounds, DrainExit, DrainOutcome, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, Dialog};

pub(crate) const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
pub(crate) const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

pub(crate) const ALICE: &str = "127.0.0.1:5060";
pub(crate) const BOB: &str = "127.0.0.1:5070";
pub(crate) const PROXY: &str = "127.0.0.1:5080";
pub(crate) const B1: &str = "127.0.0.1:5091";
pub(crate) const B2: &str = "127.0.0.1:5092";

/// The deployed shutdown bounds: the 5 s grace with the caught-up exit's 1 s
/// floor (ADR-0031 D2).
pub(crate) const GRACE: Duration = Duration::from_secs(5);
pub(crate) const FLOOR: Duration = Duration::from_millis(1000);
pub(crate) const BOUNDS: DrainBounds = DrainBounds {
    grace: GRACE,
    floor: FLOOR,
    release_flush: Duration::from_secs(3),
    cdr_flush: Duration::ZERO,
};

/// The front proxy and both workers, read alive, ready at steady state.
pub(crate) async fn bring_up(
    fh: &mut FailoverHarness,
) -> (ProxySut, ReplicatedB2buaSut, ReplicatedB2buaSut) {
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let (rt, ob) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let b1 = fh.spawn_worker("b1", "b1", B1, &["b2"], rt, ob).await;
    let b2 = fh.spawn_worker("b2", "b2", B2, &["b1"], rt, ob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready() && b2.is_ready(), "both workers ready at steady state");
    (proxy, b1, b2)
}

/// The container of `node` restarts in place: its readiness fails, so its
/// endpoint reads not ready and stays in the slice (the proxy departs it, its
/// peers keep it), and its process dies with everything it held.
pub(crate) fn restart_in_place(fh: &mut FailoverHarness, node: &mut ReplicatedB2buaSut) {
    let ordinal = node.ordinal().to_string();
    fh.flap_not_ready(&ordinal);
    fh.mark(&ordinal, None, "crash", "the container restarts: its process dies");
    node.crash();
}

/// The pod of `node` is deleted: its endpoint leaves the slice (the proxy
/// tombstones its address, its peers park it), and its process dies.
pub(crate) fn pod_deleted(fh: &mut FailoverHarness, node: &mut ReplicatedB2buaSut) {
    let ordinal = node.ordinal().to_string();
    fh.depart(&ordinal);
    fh.mark(&ordinal, None, "crash", "the pod is deleted: its process dies");
    node.crash();
}

/// `node` comes back: a fresh incarnation at a new address, published once
/// ready (the proxy reads it `Unknown`). Returns its new address.
pub(crate) async fn come_back_unknown(
    fh: &mut FailoverHarness,
    node: &mut ReplicatedB2buaSut,
) -> SocketAddr {
    let ordinal = node.ordinal().to_string();
    fh.mark(&ordinal, None, "reboot", "a fresh incarnation at a new address");
    let addr = node.reboot().await;
    fh.note_worker_rebound(&ordinal, addr);
    assert!(fh.readmit_once_ready(node).await, "{ordinal} came back ready");
    addr
}

/// [`come_back_unknown`], then the proxy reads it alive.
pub(crate) async fn come_back(
    fh: &mut FailoverHarness,
    node: &mut ReplicatedB2buaSut,
    proxy: &ProxySut,
) -> SocketAddr {
    let addr = come_back_unknown(fh, node).await;
    proxy.set_health(node.ordinal(), WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
    addr
}

/// An answered call.
pub(crate) struct Answered {
    /// The caller's dialog.
    pub(crate) dialog: Dialog,
    /// The callee's dialog.
    pub(crate) bob_dialog: Dialog,
    /// The `(w_pri, w_bak)` the proxy signed on the callee's dialog, which
    /// names the call's own primary and backup (ADR-0055 §8).
    pub(crate) cookie: (String, String),
    /// The call, as its primary stores it.
    pub(crate) call_ref: String,
}

/// A call through the proxy, answered on `primary`, the only worker that
/// takes new calls.
pub(crate) async fn answered_on(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: &ProxySut,
    primary: &ReplicatedB2buaSut,
) -> Answered {
    answered_with(fh, alice, bob, proxy, primary, None).await
}

/// [`answered_on`], with the caller's Call-ID `call_id` when one is given.
pub(crate) async fn answered_with(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: &ProxySut,
    primary: &ReplicatedB2buaSut,
    call_id: Option<&str>,
) -> Answered {
    let invite = alice.invite(bob).with_sdp(OFFER).through(proxy.addr());
    let invite = match call_id {
        Some(id) => invite.with_call_id(id),
        None => invite,
    };
    let mut call = invite.send().await;
    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    let cookie = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let bob_dialog = uas.dialog();
    let dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    // The primary keys the call by its ordinal, the caller's Call-ID and her
    // From-tag.
    let call_ref = format!("{}|{}|{}", primary.ordinal(), call.call_id(), dialog.local_tag());
    assert!(primary.serves(&call_ref), "{} serves the new call {call_ref}", primary.ordinal());
    Answered { dialog, bob_dialog, cookie, call_ref }
}

/// The caller hangs up through the proxy; the callee answers the BYE.
pub(crate) async fn caller_hangs_up(dialog: &mut Dialog, bob: &Agent) {
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// The callee hangs up through the proxy; the caller answers the BYE.
pub(crate) async fn callee_hangs_up(bob_dialog: &mut Dialog, alice: &Agent) {
    let mut bye = bob_dialog.bye().await;
    alice.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// The endpoint of `node` is withdrawn from routing and its drain runs until
/// it returns: polled after each 100 ms, never past the grace and its flush
/// bound.
pub(crate) async fn drain(fh: &mut FailoverHarness, node: &ReplicatedB2buaSut) -> DrainOutcome {
    fh.withdraw_routing(node.ordinal());
    let mut pending = fh.begin_drain_pending(node, BOUNDS);
    let bound = GRACE + BOUNDS.release_flush + Duration::from_secs(1);
    let t0 = fh.now_ms();
    loop {
        if let Some(out) = pending.poll() {
            return out;
        }
        assert!(
            fh.now_ms() - t0 <= bound.as_millis() as i64,
            "the drain of {} returned within its grace and flush bound",
            node.ordinal()
        );
        fh.advance(Duration::from_millis(100)).await;
    }
}

/// [`drain`], which must exit on its backups holding every live call, far
/// inside the grace.
pub(crate) async fn drain_caught_up(fh: &mut FailoverHarness, node: &ReplicatedB2buaSut) {
    let out = drain(fh, node).await;
    assert_eq!(
        out.exit,
        DrainExit::CaughtUp,
        "{}'s drain exits on its backup holding every live call, got {out:?}",
        node.ordinal()
    );
    assert!(out.elapsed < GRACE, "and inside the grace, got {:?}", out.elapsed);
}

/// The drained process exits: its endpoint leaves the slice and the process
/// dies.
pub(crate) async fn exits(fh: &mut FailoverHarness, node: &mut ReplicatedB2buaSut) {
    let ordinal = node.ordinal().to_string();
    fh.depart(&ordinal);
    fh.mark(&ordinal, None, "crash", "the drained process exits");
    node.crash();
    fh.advance(Duration::from_millis(300)).await;
}

/// Pump until every node holds nothing for any call, its replicas included,
/// then keep the parties draining.
pub(crate) async fn settle(
    fh: &FailoverHarness,
    nodes: &[&ReplicatedB2buaSut],
    call_refs: &[&str],
    parties: &[&Agent],
) {
    let drained = fh
        .settle_terminal(async || {
            for n in nodes {
                if !n.memory_clean() {
                    return false;
                }
                for r in call_refs {
                    if n.holds_any_trace(r).await {
                        return false;
                    }
                }
            }
            true
        })
        .await;
    assert!(drained, "the cluster releases every call, its replicas included");
    fh.linger_peers(parties, Duration::from_secs(3)).await;
}
