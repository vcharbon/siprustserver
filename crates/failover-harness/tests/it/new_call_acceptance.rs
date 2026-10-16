//! **A worker that refuses new calls keeps serving its dialogs.** A worker
//! gates its acceptance of new calls on a backend's breaker: while the breaker
//! is open it still answers the proxy's OPTIONS probe, reporting that it
//! refuses new calls, and keeps every dialog it owns. The proxy routes an
//! in-dialog request on the worker's liveness alone and a new call on its
//! acceptance (ADR-0050 §4, §6, §7).
//!
//! ```text
//!   outage   both breakers open with one call answered, one ringing
//!            the caller's BYE and the callee's BYE are relayed by their owner
//!            a new call reaches a worker, which refuses it with its own 503
//!   one      b1 refuses alone: every new call lands on b2, b1's call keeps
//!            its re-INVITE and BYE on b1, nothing is taken over
//! ```
//!
//! The probe reads a breaker flip within one probe round ([`PROBE_ROUND`]).
//! Every call ends with one CDR, the refused one included.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse,
};
use b2bua::limiter::NoopLimiter;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;
use tokio::sync::watch;

use crate::inflight_resend::{REANSWER, REOFFER};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const PROBE: &str = "127.0.0.1:5099";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// One round of the harness proxy's health probe (10 s tick) and its answer.
pub(crate) const PROBE_ROUND: Duration = Duration::from_secs(11);

/// A routing decision that is unavailable while its backend's breaker is
/// open, as a host's decision client is.
struct GatedDecision {
    breaker: watch::Receiver<bool>,
}

impl GatedDecision {
    fn unavailable(&self) -> Option<CallDecisionError> {
        (*self.breaker.borrow()).then(|| CallDecisionError::Unavailable("breaker open".into()))
    }
}

#[async_trait]
impl CallDecisionEngine for GatedDecision {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        match self.unavailable() {
            Some(e) => Err(e),
            None => Ok(NewCallResponse::Route(route_to("127.0.0.1", 5070))),
        }
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("no failure treatment".into()))
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("no transfer".into()))
    }
}

/// The two workers, each gated on its own backend breaker.
pub(crate) struct Cluster {
    pub(crate) proxy: ProxySut,
    pub(crate) b1: ReplicatedB2buaSut,
    pub(crate) b2: ReplicatedB2buaSut,
    breaker_b1: watch::Sender<bool>,
    pub(crate) breaker_b2: watch::Sender<bool>,
}

impl Cluster {
    fn node(&self, ordinal: &str) -> &ReplicatedB2buaSut {
        if ordinal == "b1" {
            &self.b1
        } else {
            &self.b2
        }
    }

    pub(crate) fn nodes(&self) -> [&ReplicatedB2buaSut; 2] {
        [&self.b1, &self.b2]
    }

    /// Takeover copies hydrated on either node so far.
    pub(crate) fn takeovers(&self) -> u64 {
        self.nodes().iter().map(|n| n.metrics().repl_takeover_hydrated_total()).sum()
    }
}

/// The proxy with its health probe and both workers, read alive by the probe.
pub(crate) async fn bring_up(fh: &mut FailoverHarness) -> Cluster {
    let proxy = fh
        .spawn_proxy_with_health_probe(
            PROXY,
            PROBE,
            &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())],
        )
        .await;
    let (rt, ob) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let (breaker_b1, open_b1) = watch::channel(false);
    let (breaker_b2, open_b2) = watch::channel(false);
    let d1: Arc<dyn CallDecisionEngine> = Arc::new(GatedDecision { breaker: open_b1.clone() });
    let d2: Arc<dyn CallDecisionEngine> = Arc::new(GatedDecision { breaker: open_b2.clone() });
    let b1 =
        fh.spawn_worker_limited("b1", "b1", B1, &["b2"], rt, ob, d1, Arc::new(NoopLimiter)).await;
    let b2 =
        fh.spawn_worker_limited("b2", "b2", B2, &["b1"], rt, ob, d2, Arc::new(NoopLimiter)).await;
    b1.gate_options_on(open_b1);
    b2.gate_options_on(open_b2);
    fh.advance(PROBE_ROUND).await;
    assert!(b1.is_ready() && b2.is_ready(), "both workers ready");
    assert_eq!(proxy.health("b1"), Some(WorkerHealth::Alive));
    assert_eq!(proxy.health("b2"), Some(WorkerHealth::Alive));
    Cluster { proxy, b1, b2, breaker_b1, breaker_b2 }
}

/// An answered call through the proxy: the caller's and the callee's dialog,
/// and the cookie's `(w_pri, w_bak)`.
async fn answered(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: &ProxySut,
) -> (Dialog, Dialog, (String, String)) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    let ordinals = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let bob_dialog = uas.dialog();
    let dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    (dialog, bob_dialog, ordinals)
}

/// The replicated call ref of the call `primary` serves, read on `backup`.
async fn call_ref_of(fh: &FailoverHarness, backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..20 {
        if let Some(call_ref) = backup.scan_one_backed_up(primary).await {
            return call_ref;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    panic!("no call of {primary} replicated to {}", backup.ordinal());
}

/// The caller hangs up through the proxy; the callee answers the BYE.
async fn caller_hangs_up(dialog: &mut Dialog, bob: &Agent) {
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// The callee hangs up through the proxy; the caller answers the BYE.
async fn callee_hangs_up(bob_dialog: &mut Dialog, alice: &Agent) {
    let mut bye = bob_dialog.bye().await;
    alice.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// Pump until both nodes hold nothing for any call.
async fn drain(fh: &FailoverHarness, c: &Cluster) {
    let drained = fh.settle_terminal(async || c.b1.memory_clean() && c.b2.memory_clean()).await;
    assert!(drained, "the cluster drains every call");
}

/// The backend is down for the whole cluster with one call answered and one
/// ringing. Neither call loses its BYE: the caller's BYE of the first and,
/// once the ringing call is answered, the callee's BYE of the second reach the
/// worker serving each. A new call placed during the outage reaches a worker,
/// which refuses it with a bare 503 and writes its record.
#[tokio::test(start_paused = true)]
async fn a_cluster_wide_backend_outage_keeps_every_dialog_and_refuses_new_calls_at_a_worker() {
    let mut fh = FailoverHarness::new("backend-outage-keeps-dialogs", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let c = bring_up(&mut fh).await;

    // ── one call answered, one ringing ──────────────────────────────────────
    let (mut first, _first_bob, (first_pri, first_bak)) =
        answered(&fh, &alice, &bob, &c.proxy).await;
    let first_ref = call_ref_of(&fh, c.node(&first_bak), &first_pri).await;
    let mut ringing = alice.invite(&bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    let (second_pri, second_bak) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    ringing.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let second_ref = call_ref_of(&fh, c.node(&second_bak), &second_pri).await;
    let takeovers = c.takeovers();

    // ── the backend goes down for both workers ──────────────────────────────
    fh.mark("b1", None, "breaker open", "the backend of both workers is down");
    c.breaker_b1.send_replace(true);
    c.breaker_b2.send_replace(true);
    fh.advance(PROBE_ROUND).await;

    // ── the caller's BYE of the answered call ───────────────────────────────
    caller_hangs_up(&mut first, &bob).await;
    assert_eq!(c.proxy.health("b1"), Some(WorkerHealth::Alive), "b1 still serves its dialogs");
    assert_eq!(c.proxy.health("b2"), Some(WorkerHealth::Alive), "b2 still serves its dialogs");

    // ── the ringing call is answered, then the callee hangs up ──────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    ringing.expect(200).await;
    let mut second_bob = uas.dialog();
    let _second = ringing.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    callee_hangs_up(&mut second_bob, &alice).await;

    // ── a new call during the outage is refused by a worker ─────────────────
    let mut refused = alice.invite(&bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    refused.expect(503).await;
    fh.advance(Duration::from_secs(1)).await;
    for reason in ["no_target_available", "no_target_selected", "worker_rate_capped"] {
        assert_eq!(
            c.proxy.metrics().reject_count(reason),
            0,
            "the proxy refused nothing: {reason}"
        );
    }

    // ── the backend returns ─────────────────────────────────────────────────
    c.breaker_b1.send_replace(false);
    c.breaker_b2.send_replace(false);
    drain(&fh, &c).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "no call was taken over");
    for call_ref in [&first_ref, &second_ref] {
        assert_eq!(total_cdrs_for(&c.nodes(), call_ref), 1, "one CDR for {call_ref}");
        assert_call_fully_released(&c.nodes(), call_ref).await;
    }
    let cdrs: usize = c.nodes().iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 3, "one CDR per call, the refused one included");
    fh.assert_sip_rfc_clean("backend-outage-keeps-dialogs");
}

/// b1's backend is down, b2's is up. Every new call lands on b2, while the
/// call b1 served before keeps its re-INVITE and its BYE on b1: no takeover.
/// The call was placed while b2 refused, so its backup was chosen among every
/// alive worker.
#[tokio::test(start_paused = true)]
async fn a_worker_refusing_new_calls_keeps_its_dialogs_and_the_others_take_the_new_ones() {
    let mut fh = FailoverHarness::new("refusing-worker-keeps-dialogs", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let c = bring_up(&mut fh).await;

    // ── b2 refuses: the call lands on b1, its backup is b2 ──────────────────
    c.breaker_b2.send_replace(true);
    fh.advance(PROBE_ROUND).await;
    let (mut held, mut held_bob, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
    assert_eq!((pri.as_str(), bak.as_str()), ("b1", "b2"), "accepting primary, refusing backup");
    let held_ref = call_ref_of(&fh, &c.b2, "b1").await;

    // ── b1 refuses, b2 accepts ──────────────────────────────────────────────
    c.breaker_b2.send_replace(false);
    c.breaker_b1.send_replace(true);
    fh.advance(PROBE_ROUND).await;
    let takeovers = c.takeovers();
    let mut new_refs = Vec::new();
    for _ in 0..3 {
        let (mut dialog, _bob_dialog, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
        assert_eq!((pri.as_str(), bak.as_str()), ("b2", "b1"), "a new call lands on b2");
        new_refs.push(call_ref_of(&fh, &c.b1, "b2").await);
        caller_hangs_up(&mut dialog, &bob).await;
    }

    // ── b1's call: a re-INVITE and the callee's BYE, all on b1 ──────────────
    let mut round = held.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect(200).await;
    held.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    assert!(c.b1.serves(&held_ref), "b1 serves its call");
    assert!(!c.b2.serves(&held_ref), "the backup never served it");
    callee_hangs_up(&mut held_bob, &alice).await;

    c.breaker_b1.send_replace(false);
    drain(&fh, &c).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "no call was taken over");
    for call_ref in std::iter::once(&held_ref).chain(&new_refs) {
        assert_eq!(total_cdrs_for(&c.nodes(), call_ref), 1, "one CDR for {call_ref}");
        assert_call_fully_released(&c.nodes(), call_ref).await;
    }
    fh.assert_sip_rfc_clean("refusing-worker-keeps-dialogs");
}
