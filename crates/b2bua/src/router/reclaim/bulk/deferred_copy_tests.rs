//! The caller's copies of the initial INVITE of an undecided call whose bulk
//! park was deferred because a turn held its lock. A copy queues behind the
//! call's arming, held while none is queued, and keeps the transaction the
//! layer opened for it until that arming rebuilds the INVITE's own: the
//! reclaimed call answers it. Whatever becomes of the call meanwhile, the
//! caller's INVITE is answered once, its retransmissions meet that answer, and
//! no copy is admitted as a new call (ADR-0022).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use call::Call;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::config::B2buaConfig;
use crate::decision::test_adapter::reject;
use crate::decision::ScriptedDecisionEngine;
use crate::initial_invite::build_initial_call;
use crate::metrics::RemovalClass;
use crate::router::test_support::{invite, node_deciding, put_body, src, Node, NODE_SIP_ADDR};
use crate::router::ReplCommand;
use crate::store::{CallGuard, CallStore, PartitionRole, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const CID: &str = "retransmitted";

/// A node whose reboot reclaim deferred the park of an undecided call of its
/// own, a turn holding its lock, the node's endpoint reading ready or, when
/// `closed`, not. Its decision, asked again, refuses the call busy.
struct Scene {
    n: Node,
    caller: Box<dyn UdpEndpoint>,
    call: Call,
    held: Option<CallGuard>,
    asked: Arc<AtomicUsize>,
}

impl Scene {
    async fn deferred(closed: bool) -> Self {
        let net = SimulatedSignalingNetwork::new(1);
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let busy = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                reject(486, "Busy Here")
            })
            .build();
        let n = node_deciding(&net, "w0", Arc::new(busy), |_| {}, Vec::new()).await;
        let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
        if closed {
            n.membership.set_conditions("w0", false, false);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut ids = sip_ids::TurnIds::fixed(1);
        let call = build_initial_call(&invite("w0", "w1", CID), src(), &config, &mut ids, 0);
        put_body(&n.store, PRI, "w0", &call).await;
        let held = Some(n.core.router_ctx().state.lock(&call.call_ref).await);
        n.core.fail_back_sender().expect("a wired node").send(ReplCommand::ReclaimAll).unwrap();
        sip_clock::testkit::settle().await;
        Self { n, caller, call, held, asked }
    }

    /// The caller sends `raw` to the node.
    async fn send(&self, raw: &[u8]) {
        self.caller.send_to(raw, SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    /// The caller retransmits its INVITE.
    async fn retransmit(&self) {
        self.send(invite("w0", "w1", CID).image()).await;
    }

    /// The turn holding the call's lock lets it go.
    async fn let_go(&mut self) {
        drop(self.held.take());
        sip_clock::testkit::settle().await;
    }

    fn open(&self) {
        self.n.membership.set_conditions("w0", true, false);
    }

    /// The finals the caller receives within half a second, whole.
    async fn finals(&self) -> Vec<String> {
        let mut finals = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while let Ok(Some(p)) = tokio::time::timeout_at(deadline, self.caller.recv()).await {
            let text = String::from_utf8_lossy(&p.raw).to_string();
            if !text.starts_with("SIP/2.0 1") {
                finals.push(text);
            }
        }
        finals
    }

    /// The caller's INVITE draws one final, `status`, which its transaction
    /// repeats until the ACK and its next retransmission meets again; the
    /// caller ACKs it.
    async fn answered_once(&self, status: &str) {
        let finals = self.finals().await;
        assert!(!finals.is_empty(), "a final reaches the caller");
        assert!(finals.iter().all(|f| *f == finals[0]), "one final, repeated: {finals:?}");
        assert!(finals[0].starts_with(status), "{status}: {finals:?}");
        self.retransmit().await;
        let again = self.finals().await;
        assert!(!again.is_empty(), "the retransmission is answered");
        assert!(again.iter().all(|f| *f == finals[0]), "with the same final: {again:?}");
        self.send(&ack_of(&finals[0])).await;
    }

    /// No copy was admitted as a new call, and nothing is left once the
    /// INVITE's transaction ended.
    async fn no_new_call(&self, cdrs: usize) {
        let ctx = self.n.core.router_ctx();
        let counts = crate::new_calls::NewCallCounts::compose(
            ctx.metrics.new_calls(),
            Default::default(),
            Default::default(),
        );
        assert_eq!(counts.total(), 0, "no copy is a new call");
        let written =
            self.n.cdr.snapshot().into_iter().filter(|c| c.call_ref == self.call.call_ref);
        assert_eq!(written.count(), cdrs, "the reclaimed call's CDRs");
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(ctx.state.active_count(), 0, "no call is left");
        assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
        assert_eq!(self.n.metrics.timer_live(), 0, "no timer is left behind");
    }

    /// The call's record is gone from the store: it ended under the holding
    /// turn, or its TTL ran out.
    async fn record_gone(&self) {
        let keys = call::call_index_keys(&self.call);
        let opts = PutOpts::default();
        let r = &self.call.call_ref;
        self.n.store.delete_call(PRI, "w0", r, &keys, false, &opts).await.unwrap();
    }
}

/// The caller's ACK of the non-2xx final `response` to its INVITE: the
/// INVITE's branch, the final's To (RFC 3261 §17.1.1.3).
fn ack_of(response: &str) -> Vec<u8> {
    let to = response.lines().find(|l| l.starts_with("To:")).expect("the final's To");
    format!(
        "ACK sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{CID}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         {to}\r\n\
         Call-ID: {CID}@10.0.0.9\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The caller's CANCEL of its INVITE (RFC 3261 §9.1).
fn cancel() -> Vec<u8> {
    format!(
        "CANCEL sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{CID}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {CID}@10.0.0.9\r\n\
         CSeq: 1 CANCEL\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_retransmission_of_a_deferred_calls_invite_is_served_by_the_reclaimed_call() {
    let mut s = Scene::deferred(false).await;
    s.retransmit().await;
    assert_eq!(s.n.core.router_ctx().unborn.count(), 0, "admitted as no new call");
    s.let_go().await;

    s.answered_once("SIP/2.0 486").await;
    assert_eq!(s.asked.load(Ordering::SeqCst), 1, "the decision asked again once");
    s.no_new_call(1).await;
}

/// Behind a closed gate the retransmissions wait for the arming, which seeds
/// the INVITE's transaction and asks the undecided call's decision again.
#[tokio::test(start_paused = true)]
async fn retransmissions_behind_a_closed_gate_wait_for_the_deferred_calls_arming() {
    let mut s = Scene::deferred(true).await;
    s.retransmit().await;
    s.retransmit().await;
    s.let_go().await;
    assert!(s.finals().await.is_empty(), "no final before the arming");
    assert!(s.n.core.router_ctx().state.is_parked(&s.call.call_ref), "parked, not armed");

    s.open();
    s.answered_once("SIP/2.0 486").await;
    assert_eq!(s.asked.load(Ordering::SeqCst), 1, "the decision asked again once");
    s.no_new_call(1).await;
}

/// The record went while a copy waited for the arming: the park finds no
/// call, and the copy is answered as one of a call that is gone.
#[tokio::test(start_paused = true)]
async fn a_copy_held_for_a_call_gone_by_its_park_is_answered_once() {
    let mut s = Scene::deferred(true).await;
    s.retransmit().await;
    s.record_gone().await;
    s.let_go().await;
    s.open();

    s.answered_once("SIP/2.0 481").await;
    assert_eq!(s.asked.load(Ordering::SeqCst), 0, "no decision is asked");
    s.no_new_call(0).await;
}

/// The holding turn ended the call and queued its release: the arming meets
/// the released queue, and the copy it held is answered as one of a call
/// that ended.
#[tokio::test(start_paused = true)]
async fn a_copy_held_for_a_call_whose_arming_meets_its_release_is_answered_once() {
    let mut s = Scene::deferred(true).await;
    s.retransmit().await;
    s.record_gone().await;
    let r = s.call.call_ref.clone();
    s.n.core.router_ctx().dispatcher.release(&r, RemovalClass::Terminated);
    s.open();
    tokio::time::sleep(Duration::from_millis(100)).await;
    s.let_go().await;

    s.answered_once("SIP/2.0 481").await;
    assert_eq!(s.asked.load(Ordering::SeqCst), 0, "no decision is asked");
    s.no_new_call(0).await;
}

/// The caller CANCELs its INVITE while a copy waits for the arming: the
/// setup ends cancelled, its decision never asked.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_a_copy_waits_ends_the_setup_unasked() {
    let mut s = Scene::deferred(true).await;
    s.retransmit().await;
    s.send(&cancel()).await;
    s.let_go().await;
    s.open();

    let finals = s.finals().await;
    let cancel_ok = finals.iter().filter(|f| f.contains("CSeq: 1 CANCEL")).count();
    let invite_finals: Vec<&String> =
        finals.iter().filter(|f| f.contains("CSeq: 1 INVITE")).collect();
    assert!(cancel_ok >= 1, "the CANCEL is answered: {finals:?}");
    assert!(!invite_finals.is_empty(), "the INVITE is answered: {finals:?}");
    assert!(invite_finals.iter().all(|f| f.starts_with("SIP/2.0 487")), "487 alone: {finals:?}");
    s.send(&ack_of(invite_finals[0])).await;
    assert_eq!(s.asked.load(Ordering::SeqCst), 0, "no decision is asked");
    s.no_new_call(1).await;
}
