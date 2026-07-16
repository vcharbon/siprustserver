//! The fluent session: [`Harness`] owns the recording-wrapped simulated
//! network, binds agents/proxies/SUTs, advances virtual time, and renders the
//! [`RunReport`] at [`Harness::finish`] behind the mandatory RFC hard gate.
//! The Drop-armed backstops live in [`super::run_guards`].

use std::cell::RefCell;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{NetworkTag, Recorder, RunContext, TransportKind};
use sip_clock::Clock;
use sip_net::{
    with_all_contracts, BindUdpOpts, ScopedAuditOptions, SignalingNetwork, UdpEndpoint,
};

use super::run_guards::{render_rfc_panic, rfc_hard_gate_findings, CseqGate, PanicDump};
use super::rr_fold::decide_rr_fold;
use super::txn_view::{AckObligations, TxnView};
use super::{Agent, Proxy};
use crate::run::RunReport;

const RECV_TIMEOUT: Duration = Duration::from_secs(2);

/// Monotonic id source for branches / tags / Call-IDs. Deterministic (no RNG),
/// so report bytes are stable across runs. `pub(crate)` so the Send
/// [`loadbind::AgentBinder`](crate::loadbind::AgentBinder) can share an id
/// source the same way [`Harness`] does.
pub(crate) struct Ids(pub(crate) AtomicU64);
impl Ids {
    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// A running fluent session: owns the recording-wrapped simulated network and
/// hands out [`Agent`]s. Drop or [`finish`](Harness::finish) when done.
pub struct Harness {
    network: Arc<dyn SignalingNetwork>,
    recording: sip_net::RecordingSignalingNetwork,
    recorder: Recorder,
    ids: Arc<Ids>,
    name: String,
    description: Option<String>,
    /// Dumps the recorded trace to stderr if the scenario task unwinds before
    /// [`finish`](Harness::finish) renders a report. `finish` disarms it.
    dump: PanicDump,
    /// MANDATORY HARD GATE: fails the test on Drop if the recorded trace violates
    /// any non-advisory RFC rule — the backstop for a harness dropped WITHOUT
    /// [`finish`](Harness::finish) (which enforces the same gate inline).
    /// `finish` disarms it (it has already run the gate itself).
    cseq_gate: CseqGate,
    /// Rules a test is allowed to violate (a deliberate non-compliance fixture
    /// where Alice/Bob intentionally emit non-conforming SIP). The hard gate
    /// skips findings from these rule names. Shared (`Rc`) with [`CseqGate`] so a
    /// `allow_violation` registered before `finish`/Drop is honoured by both.
    allow_violations: Rc<RefCell<HashSet<String>>>,
    /// Per-`recv` wait bound handed to every [`Agent`] this harness binds. Small
    /// under a paused (simulated) clock — a parked `recv` auto-advances virtual
    /// time, so the bound only catches a genuinely stuck flow. A *real*-clock
    /// infra shape (an external SUT over real sockets) must widen it, since the
    /// wait is then real wall-clock latency. Sourced from the Endpoint config.
    recv_timeout: Duration,
    /// `(agent, anchor)` message labels a scenario attached via
    /// [`tag_anchor`](Harness::tag_anchor), surfaced on the [`RunReport`] for
    /// the E2E check engine (ADR-0019).
    anchors: Rc<RefCell<Vec<crate::anchors::AnchorTag>>>,
}

impl Harness {
    /// Start a session named `scenario_name`. The simulated fabric uses the
    /// default [`crate::SIMULATED_TRANSIT_DELAY_MS`] one-hop transit delay, so a
    /// sent datagram arrives that much later (mirrors a real network).
    /// Timestamps ride a test clock so a paused runtime gives deterministic
    /// report times (see `run::run`).
    pub fn new(scenario_name: impl Into<String>) -> Self {
        Self::with_transit_delay(scenario_name, crate::SIMULATED_TRANSIT_DELAY_MS)
    }

    /// Like [`new`](Harness::new) but with an explicit one-hop transit delay
    /// (ms). A delay of `0` is coerced to `1`: zero transit under a paused
    /// runtime is a determinism trap (the delivery `sleep(0)` races the txn →
    /// router → dispatcher pipeline, so timer cancels land after the timer
    /// fired). A non-zero delay makes each `recv` park auto-advance
    /// deterministically. See [`sip_net::SimulatedSignalingNetwork::new`].
    pub fn with_transit_delay(scenario_name: impl Into<String>, transit_delay_ms: u64) -> Self {
        let transit_delay_ms = transit_delay_ms.max(1);
        let sim: Arc<dyn SignalingNetwork> =
            Arc::new(sip_net::SimulatedSignalingNetwork::new(transit_delay_ms));
        Self::build(
            scenario_name.into(),
            sim,
            Clock::test_at(0),
            TransportKind::Fake,
            RECV_TIMEOUT,
        )
    }

    /// Start a session over a **caller-supplied** network + clock — the seam an
    /// E2E *Infra shape* uses to run the **same** scenario over either a
    /// `SimulatedSignalingNetwork` under a paused clock (fake) or a
    /// `RealSignalingNetwork` under a wall clock (real, external SUT). The only
    /// per-shape differences are transport + clock + the `recv_timeout`; the
    /// scenario body is identical (ADR-0018). `transport_kind` tags the recording
    /// (`Fake` / `Live` / `Hybrid`); `recv_timeout` comes from the Endpoint config
    /// (small for sim, wide for a real socket).
    pub fn with_network_and_clock(
        scenario_name: impl Into<String>,
        network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        transport_kind: TransportKind,
        recv_timeout: Duration,
    ) -> Self {
        Self::build(
            scenario_name.into(),
            network,
            clock,
            transport_kind,
            recv_timeout,
        )
    }

    /// Shared constructor body: wrap the raw network in the recorder + the full
    /// default RFC 3261 / 3262 / 3264 wire-invariant suite (per-message peer rules
    /// + cross-message rules) so every harness run gets the same post-run "all
    /// clean" RFC check the live SIPp endpoints apply — a stale-CSeq probe, a
    /// mid-dialog tag/route mutation, an RFC 3262/3264 PRACK/offer-answer slip that
    /// a test UA would silently answer is caught at layer close.
    fn build(
        name: String,
        raw_network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        transport_kind: TransportKind,
        recv_timeout: Duration,
    ) -> Self {
        let recorder = Recorder::with_clock(transport_kind, clock);
        let audit_opts = ScopedAuditOptions {
            rules: sip_net::rfc_peer_rules(),
            cross_message_rules: sip_net::rfc_cross_message_rules(),
            ..Default::default()
        };
        let wrapped = with_all_contracts(
            raw_network,
            recorder.clone(),
            RunContext::TestWithRecorder,
            audit_opts,
            true,
        );
        let dump = PanicDump::new(name.clone(), wrapped.recording.channel(), recorder.clone());
        let allow_violations: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
        let cseq_gate = CseqGate::new(
            name.clone(),
            wrapped.recording.channel(),
            allow_violations.clone(),
        );
        Self {
            network: wrapped.network,
            recording: wrapped.recording,
            recorder,
            ids: Arc::new(Ids(AtomicU64::new(1))),
            name,
            description: None,
            dump,
            cseq_gate,
            allow_violations,
            recv_timeout,
            anchors: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Re-seed the shared branch/tag/Call-ID counter. The default (1) is
    /// deterministic so fake-fabric report bytes are stable across runs — but
    /// against a REAL, stateful, shared SUT (the kind cluster) every run would
    /// then mint the SAME Call-IDs and Via branches as the last one, and the
    /// SUT's transaction layer rightly treats the new INVITE as a
    /// retransmission of the finished call and replays its cached final
    /// response (RFC 3261 §17.2.3 absorption — observed as an instant 200 OK
    /// and a never-arriving b-leg). A real Infra shape seeds with wall-clock
    /// entropy so identifiers are unique across runs, as RFC 3261 §8.1.1.4
    /// demands of a real UA.
    pub fn seed_ids(&self, seed: u64) {
        self.ids.0.store(seed, Ordering::Relaxed);
    }

    /// Label a message `agent` received with a canonical **anchor** name
    /// (ADR-0019) — e.g. `tag_anchor(&bob1, "initialInvite", uas.request())`.
    /// The tag stores the message's identity keys; the E2E check engine
    /// resolves `<agent>.<anchor>` to the recorded wire entry post-call via
    /// [`RunReport::anchors`]. Tagging the same `(agent, anchor)` twice keeps
    /// both (resolution takes the first — re-tag deliberately, not by accident).
    pub fn tag_anchor(
        &self,
        agent: &Agent,
        anchor: impl Into<String>,
        keys: impl Into<crate::anchors::AnchorKeys>,
    ) {
        self.anchors.borrow_mut().push(crate::anchors::AnchorTag {
            agent: agent.name().to_string(),
            anchor: anchor.into(),
            agent_addr: agent.addr(),
            keys: keys.into(),
            sent: false,
        });
    }

    /// Set the report description.
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Allow a single RFC rule to be violated on this run WITHOUT failing the
    /// hard gate — the **only** sanctioned way to deactivate a rule in a test.
    ///
    /// Reserve this for a deliberate non-compliance fixture, where Alice/Bob
    /// intentionally emit non-conforming SIP to exercise the B2BUA's reject /
    /// error path (e.g. an out-of-dialog BYE to drive a 481). The `justification`
    /// is mandatory and logged. It is **not** an escape hatch for a finding that
    /// reflects a real B2BUA bug — fix the B2BUA (or the test peer) instead.
    ///
    /// `rule` is the rule's `name()` (e.g. `"rfc3261.byeOnlyInDialog"`). The
    /// finding is still recorded (advisory) so the report shows what was waived.
    pub fn allow_violation(&self, rule: impl Into<String>, justification: impl Into<String>) {
        let rule = rule.into();
        let justification = justification.into();
        eprintln!(
            "[harness] RFC rule '{rule}' allowed to be violated on '{}': {justification}",
            self.name
        );
        self.allow_violations.borrow_mut().insert(rule);
    }

    /// Disarm the Drop-time RFC 3261 CSeq hard gate. For multi-SUT harnesses
    /// (e.g. the failover matrix) that wrap this `Harness` and run their OWN,
    /// **bind-scoped** CSeq audit at their layer-close: the generic gate here is
    /// unscoped — it audits every recorded bind, including the internal cluster
    /// workers, where a *transparent* failover legitimately splits one logical
    /// dialog's CSeq stream across nodes (CSeq 1 → primary, 2 → backup-takeover,
    /// 3 → reclaimed primary) and so reports a phantom "skip" no real UA sees.
    /// The wrapping harness substitutes the correct endpoint-scoped check, so it
    /// disarms this one to avoid a redundant, unscoped double-gate.
    pub fn disarm_cseq_gate(&self) {
        self.cseq_gate.disarm();
    }

    /// Declare and bind a named UA at `addr` (e.g. `"127.0.0.1:5060"`). The
    /// bind is role-tagged `{Uac, Uas}` — a test UA originates and answers but
    /// is never a proxy, so proxy-subject RFC rules (no-target-404,
    /// 100-within-200ms, strict-route rewrite, PRACK forwarding) do not judge
    /// its lane. Use [`agent_with_roles`](Self::agent_with_roles) to override.
    pub async fn agent(&self, name: impl Into<String>, addr: &str) -> Agent {
        self.agent_with_roles(name, addr, HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]))
            .await
    }

    /// [`agent`](Self::agent) with explicit bind roles for RFC-rule subject
    /// dispatch (e.g. a UA scripted to relay should declare `{Proxy}` too).
    pub async fn agent_with_roles(
        &self,
        name: impl Into<String>,
        addr: &str,
        roles: HashSet<sip_net::UaRole>,
    ) -> Agent {
        self.agent_with_opts(name, addr, roles, None).await
    }

    /// [`agent`](Self::agent) (default UAC/UAS roles) with an arrival-time
    /// [`sip_net::PreIngressHook`] on the UA's bind — the deterministic
    /// loss-model seam for scenario tests: drop selected datagrams BEFORE they
    /// reach this agent's inbox (e.g. "lose the first copy of the non-2xx
    /// final", forcing the peer's Timer G retransmit path). The fabric still
    /// records the send; the drop is counted on the bind's
    /// `pre_ingress_dropped`.
    pub async fn agent_with_pre_ingress(
        &self,
        name: impl Into<String>,
        addr: &str,
        hook: sip_net::PreIngressHook,
    ) -> Agent {
        self.agent_with_opts(
            name,
            addr,
            HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]),
            Some(hook),
        )
        .await
    }

    async fn agent_with_opts(
        &self,
        name: impl Into<String>,
        addr: &str,
        roles: HashSet<sip_net::UaRole>,
        pre_ingress: Option<sip_net::PreIngressHook>,
    ) -> Agent {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name.clone(), NetworkTag::Ext);
        let mut opts = BindUdpOpts::new(addr, 64).with_roles(roles);
        if let Some(hook) = pre_ingress {
            opts = opts.with_pre_ingress(hook);
        }
        let ep = self
            .network
            .bind_udp(opts)
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        let rr_fold = decide_rr_fold(&name);
        eprintln!(
            "[harness] UA {name}: Record-Route fold = {rr_fold:?} \
             (set HARNESS_RR_FOLD=separate|combined to pin)"
        );
        Agent {
            name: name.clone(),
            addr,
            uri: format!("sip:{name}@{}", addr.ip()),
            ep: Arc::from(ep),
            ids: self.ids.clone(),
            rr_fold,
            recv_timeout: self.recv_timeout,
            txn: Arc::new(TxnView::functional()),
            acks: Arc::new(AckObligations::default()),
        }
    }

    /// Begin a **multi-callee group** — several logical [`Agent`]s that share
    /// ONE bound socket at `addr`, demultiplexed by R-URI prefix (out-of-dialog)
    /// and Call-ID (in-dialog). This is how the transfer flows drive Bob /
    /// Charlie / David when the B2BUA egresses every callee leg to a single ROUTE
    /// target: `callee_group(addr).callee("bob", "049…").callee("charlie", "231…")
    /// .build().await`. See [`crate::callee_group`]. Both the direct and the
    /// via-proxy paths are supported (the demux is source-address-agnostic).
    pub fn callee_group(&self, addr: &str) -> crate::callee_group::CalleeGroupBuilder<'_> {
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        crate::callee_group::CalleeGroupBuilder::new(self, addr)
    }

    /// The shared recording-wrapped network — so the `callee_group` builder binds
    /// its one shared socket through the very fabric the agents use.
    pub(crate) fn network(&self) -> Arc<dyn SignalingNetwork> {
        self.network.clone()
    }

    /// Register a recorder lane (one per bound socket) — used by
    /// [`agent_with_roles`](Self::agent_with_roles) and the `callee_group`
    /// builder, which registers a single lane for the shared callee socket.
    pub(crate) fn register_lane(&self, addr: SocketAddr, name: String, tag: NetworkTag) {
        self.recorder.register_lane(addr, name, tag);
    }

    /// The shared monotonic id source, so `callee_group`'s logical agents mint
    /// branches/tags/Call-IDs from the same deterministic sequence as every other
    /// agent this harness binds.
    pub(crate) fn ids(&self) -> Arc<Ids> {
        self.ids.clone()
    }

    /// The per-`recv` wait bound handed to every agent (inherited by a
    /// `callee_group`'s logical agents).
    pub(crate) fn recv_timeout(&self) -> Duration {
        self.recv_timeout
    }

    /// Declare and bind a record-routing proxy / load balancer at `addr`.
    /// Role-tagged `{Proxy}` so the proxy-subject RFC rules judge this lane
    /// and the per-UA dialog rules do not.
    pub async fn proxy(&self, name: impl Into<String>, addr: &str) -> Proxy {
        Proxy::new(
            self.agent_with_roles(name, addr, HashSet::from([sip_net::UaRole::Proxy]))
                .await,
        )
    }

    /// Bind a **System-Under-Test** endpoint on the shared, recording-wrapped
    /// fabric and register a `Core` lane for it. Returns the raw
    /// [`UdpEndpoint`] + its bound address so a real SUT (e.g. `sip-proxy`'s
    /// `ProxyCore`) can run its own recv loop against the same network the
    /// agents use — every `send_to`/`recv` still flows through the recorder, so
    /// the recording remains the trace. The caller owns the spawned loop (abort
    /// it on drop). This is the seam that lets the harness drive a real proxy,
    /// not just peer-to-peer agents (ADR-0006 → ADR-0009).
    pub async fn bind_sut(&self, name: impl Into<String>, addr: &str) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        self.bind_sut_with_roles(name, addr, sip_net::all_ua_roles()).await
    }

    /// [`bind_sut`](Self::bind_sut) with explicit bind roles for RFC-rule
    /// subject dispatch: a `ProxyCore` SUT declares `{Proxy}`, a B2BUA SUT
    /// `{Uac, Uas}` (it terminates each leg as a UA — proxy-subject rules like
    /// no-target-404 do not govern it). The roles ride the recorded
    /// `BindAcquire` summary, so the hard gate and the report projection
    /// dispatch rules per endpoint with no extra plumbing.
    pub async fn bind_sut_with_roles(
        &self,
        name: impl Into<String>,
        addr: &str,
        roles: HashSet<sip_net::UaRole>,
    ) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name, NetworkTag::Core);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 256).with_roles(roles))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        (ep, addr)
    }

    /// Advance virtual time by `d` (requires a paused runtime —
    /// `#[tokio::test(start_paused = true)]`). Advances in 100 ms chunks so
    /// in-flight delivery tasks observe intermediate instants. Because the
    /// report's `at_ms` rides the same tokio clock (via `sip-clock`), the
    /// elapsed time shows up in the rendered timestamps. Call it *between*
    /// protocol events (after the message just sent has been `expect`ed) so
    /// each message keeps a clean send/receive timestamp.
    pub async fn advance(&self, d: Duration) {
        sip_clock::testkit::advance_in_100ms_chunks(d).await;
    }

    /// Drain the fabric before the trace snapshot: wait out in-flight
    /// datagrams (a txn-layer auto-ACK sent by the scenario's LAST receive is
    /// still in transit when `finish` runs), then yield the scheduler so a
    /// LIVE recv loop (the SUT's) reads what was delivered. Fabricates
    /// nothing: a passive test agent's queue is untouched — its unread
    /// datagrams are already recorded at DELIVERY (arrival is a wire fact,
    /// tagged `unconsumed` on the ladder), so the audit sees them without any
    /// explicit read.
    async fn settle_network(&self) {
        self.network.await_in_flight(Duration::from_millis(200)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // The reads above may themselves send (an auto-answered keepalive) —
        // one more drain so the snapshot closes on a quiet wire.
        self.network.await_in_flight(Duration::from_millis(200)).await;
    }

    /// The recording decorator handle — lets a caller read the raw signaling
    /// event channel (`recording().channel().snapshot()`) to run an audit rule
    /// directly over the trace WITHOUT consuming the harness or invoking the
    /// structural layer-close checks. Used by long-lived multi-SUT harnesses
    /// (e.g. the failover matrix) that assert the RFC CSeq check mid-life.
    pub fn recording(&self) -> sip_net::RecordingSignalingNetwork {
        self.recording.clone()
    }

    /// Close the recording layer and return the [`RunReport`] (trace projected
    /// from the recording). Failures in the fluent flow panic in-line, so a
    /// returned report is by construction a passing run.
    ///
    /// MANDATORY HARD GATE: before returning, the recorded trace is checked against
    /// the RFC 3261 in-dialog CSeq rule(s); ANY finding `panic!`s, failing the
    /// test. This is the SIP-plane analogue of the universal "all clean" sweep,
    /// applied to EVERY harness run with no per-test opt-in. Only the cseq
    /// cross-message rules gate here — NOT the structural `close()` anomalies
    /// (inFlightImbalance / queueLeak), which legitimately occur in timeout / reap
    /// / stall fixtures and must not fail those tests.
    pub async fn finish(self) -> RunReport {
        // A returned report is by construction a passing run, so the post-mortem
        // dump is no longer wanted: disarm it before tearing the session down. The
        // Drop-time cseq backstop is likewise disarmed — `finish` runs the SAME
        // gate inline just below, so the Drop guard would only double-check.
        self.dump.disarm();
        self.cseq_gate.disarm();
        self.settle_network().await;
        let events = self.recording.channel().snapshot();
        // Hard gate on the RFC CSeq rule(s) BEFORE the structural close: a CSeq
        // violation must fail the test (a real UA would reject these). Skip if the
        // test is already unwinding so we never double-panic. The structural
        // close() anomalies are intentionally NOT gated.
        let cseq_findings = rfc_hard_gate_findings(&events, &self.allow_violations.borrow());
        if !cseq_findings.is_empty() && !std::thread::panicking() {
            panic!("{}", render_rfc_panic(&self.name, &cseq_findings));
        }
        let audit = self.recording.close().await;
        let anchors = self.anchors.borrow().clone();
        RunReport::from_recording(self.name, self.description, self.recorder, events, audit, anchors)
    }

    /// [`finish`](Self::finish) that **collects** the hard-gate findings
    /// instead of panicking — for executors (the e2e run core) that want a
    /// gating RFC violation to FAIL the cell *with the full report intact*
    /// (diagram, checks, findings table) rather than crash it report-less.
    /// The returned findings are exactly the set `finish` would have panicked
    /// on: non-advisory, subject-applicable to the originating bind's declared
    /// roles, and not waived via [`allow_violation`](Self::allow_violation).
    pub async fn finish_collecting(self) -> (RunReport, Vec<sip_net::RfcFinding>) {
        self.dump.disarm();
        self.cseq_gate.disarm();
        self.settle_network().await;
        let events = self.recording.channel().snapshot();
        let allow = self.allow_violations.borrow().clone();
        let gating: Vec<sip_net::RfcFinding> = sip_net::evaluate_rfc_findings(&events)
            .into_iter()
            .filter(|f| !f.advisory && !allow.contains(&f.rule))
            .collect();
        let audit = self.recording.close().await;
        let anchors = self.anchors.borrow().clone();
        let report = RunReport::from_recording(
            self.name,
            self.description,
            self.recorder,
            events,
            audit,
            anchors,
        );
        (report, gating)
    }
}
