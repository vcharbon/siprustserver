//! The fluent, dialog-aware harness — the auto-generating DSL (port of the
//! load-bearing half of `recorder.ts`'s `AgentProxy` / `DialogRef` + the dialog
//! state in `message-builder.ts`).
//!
//! This is the layer that means a scenario does **not** hand-author headers.
//! Agents are stateful UAs; each high-level call generates a correct-by-default
//! B2B message via `sip_message::generators` and tracks the dialog state needed
//! for the next one:
//!
//! ```ignore
//! let h = Harness::new("alice-calls-bob");
//! let alice = h.agent("alice", "127.0.0.1:5060").await;
//! let bob   = h.agent("bob",   "127.0.0.1:5070").await;
//!
//! let mut call = alice.invite(&bob).with_sdp(OFFER).send().await; // INVITE auto-built
//! let mut uas  = bob.receive("INVITE").await;
//! uas.respond(180, "Ringing").await;                              // To-tag minted here
//! call.expect(180).await;                                         // learns remote tag/target
//! uas.respond(200, "OK").with_sdp(ANSWER).await;
//! call.expect(200).await;
//! let mut dialog = call.ack().await;                              // ACK reuses INVITE CSeq
//! bob.receive("ACK").await;
//! let mut bye = dialog.bye().await;                               // BYE auto-increments CSeq
//! bob.receive("BYE").await.respond(200, "OK").await;
//! bye.expect(200).await;
//! let report = h.finish().await;                                  // render from the recording
//! ```
//!
//! What the harness fills in automatically, per RFC 3261: Via (fresh branch per
//! transaction, magic cookie), From/To with tags, Call-ID continuity, CSeq
//! numbering (1 INVITE → 1 ACK → n BYE; responses echo), Contact, Max-Forwards,
//! Content-Type/Length, remote-target routing (in-dialog requests go to the
//! peer's Contact). Everything still flows through the recording-wrapped
//! `SignalingNetwork`, so the reports are projected from the record exactly as
//! before — the auto-generation only changes *who writes the bytes*.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Channel, NetworkTag, Recorder, RunContext, TransportKind};
use sip_clock::Clock;
use sip_message::generators::{
    generate_ack_for_2xx, generate_cancel, generate_in_dialog_request,
    generate_out_of_dialog_request,
    generate_response, strip_route_uri_to_request_uri, ContactSpec, GenerateAckFor2xxOpts,
    GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts,
    InDialogMethod, InviteClientTransactionHandle, OutOfDialogMethod, SipTransport, StackDialog,
    ViaSpec,
};
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::{
    to_sip_entries, with_all_contracts, BindUdpOpts, ScopedAuditOptions, SignalingNetwork,
    SignalingNetworkEvent, SimulatedSignalingNetwork, UdpEndpoint,
};

use crate::report::wire::{facets, format_relative};
use crate::run::RunReport;

const RECV_TIMEOUT: Duration = Duration::from_secs(2);

/// Monotonic id source for branches / tags / Call-IDs. Deterministic (no RNG),
/// so report bytes are stable across runs.
struct Ids(AtomicU64);
impl Ids {
    fn next(&self) -> u64 {
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
    /// the RFC 3261 in-dialog CSeq rule — the backstop for a harness dropped
    /// WITHOUT [`finish`](Harness::finish) (which enforces the same gate inline).
    /// `finish` disarms it (it has already run the gate itself).
    cseq_gate: CseqGate,
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
    /// deterministically. See [`SimulatedSignalingNetwork::new`].
    pub fn with_transit_delay(scenario_name: impl Into<String>, transit_delay_ms: u64) -> Self {
        let transit_delay_ms = transit_delay_ms.max(1);
        let recorder = Recorder::with_clock(TransportKind::Fake, Clock::test_at(0));
        let sim = Arc::new(SimulatedSignalingNetwork::new(transit_delay_ms));
        // Install the built-in RFC 3261 wire-invariant rules (CSeq in-dialog
        // ordering, …) by default so every harness run gets the same post-run
        // "all clean" check the live SIPp endpoints apply — a stale-CSeq probe a
        // test UA would silently answer is caught at layer close.
        let audit_opts = ScopedAuditOptions {
            cross_message_rules: sip_net::rfc_cross_message_rules(),
            ..Default::default()
        };
        let wrapped = with_all_contracts(
            sim,
            recorder.clone(),
            RunContext::TestWithRecorder,
            audit_opts,
            true,
        );
        let name = scenario_name.into();
        let dump = PanicDump {
            name: name.clone(),
            channel: wrapped.recording.channel(),
            recorder: recorder.clone(),
            armed: Cell::new(true),
        };
        let cseq_gate = CseqGate {
            name: name.clone(),
            channel: wrapped.recording.channel(),
            armed: Cell::new(true),
        };
        Self {
            network: wrapped.network,
            recording: wrapped.recording,
            recorder,
            ids: Arc::new(Ids(AtomicU64::new(1))),
            name,
            description: None,
            dump,
            cseq_gate,
        }
    }

    /// Set the report description (port of `.describe(...)`).
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
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

    /// Declare and bind a named UA at `addr` (e.g. `"127.0.0.1:5060"`).
    pub async fn agent(&self, name: impl Into<String>, addr: &str) -> Agent {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name.clone(), NetworkTag::Ext);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 64))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        Agent {
            name: name.clone(),
            addr,
            uri: format!("sip:{name}@{}", addr.ip()),
            ep: Arc::from(ep),
            ids: self.ids.clone(),
        }
    }

    /// Declare and bind a record-routing proxy / load balancer at `addr`.
    pub async fn proxy(&self, name: impl Into<String>, addr: &str) -> Proxy {
        Proxy {
            agent: self.agent(name, addr).await,
        }
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
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name, NetworkTag::Core);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 256))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        (ep, addr)
    }

    /// Advance virtual time by `d` (requires a paused runtime —
    /// `#[tokio::test(start_paused = true)]`). Advances in 100 ms chunks,
    /// mirroring the source's `TestClock.adjust` loop so in-flight delivery
    /// tasks observe intermediate values. Because the report's `at_ms` rides
    /// the same tokio clock (via `sip-clock`), the elapsed time shows up in the
    /// rendered timestamps. Call it *between* protocol events (after the message
    /// just sent has been `expect`ed) so each message keeps a clean send/receive
    /// timestamp.
    pub async fn advance(&self, d: Duration) {
        sip_clock::testkit::advance_in_100ms_chunks(d).await;
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
        let events = self.recording.channel().snapshot();
        // Hard gate on the RFC CSeq rule(s) BEFORE the structural close: a CSeq
        // violation must fail the test (a real UA would reject these). Skip if the
        // test is already unwinding so we never double-panic. The structural
        // close() anomalies are intentionally NOT gated.
        let cseq_findings = rfc_cseq_findings(&events);
        if !cseq_findings.is_empty() && !std::thread::panicking() {
            panic!("{}", render_cseq_panic(&self.name, &cseq_findings));
        }
        let audit = self.recording.close().await;
        RunReport::from_recording(self.name, self.description, self.recorder, events, audit)
    }
}

/// The RFC 3261 cross-message (in-dialog CSeq) findings over a recorded trace —
/// the `(lane, detail)` pairs the hard gate fails on. ONLY the cross-message
/// rules run here (the structural layer-close anomalies are deliberately not
/// consulted), so timeout / reap / stall fixtures that legitimately leave an
/// in-flight imbalance are not gated. Shared by `Harness::finish` and the
/// `Harness` Drop guard so the SAME rule set runs on every run with no per-test
/// opt-in. Empty ⇒ clean.
fn rfc_cseq_findings(
    events: &[layer_harness::Stamped<SignalingNetworkEvent>],
) -> Vec<(String, String)> {
    let mut findings = Vec::new();
    for rule in sip_net::rfc_cross_message_rules() {
        findings.extend(rule.check(events));
    }
    findings
}

/// Format the hard-gate panic message listing every RFC CSeq violation.
fn render_cseq_panic(name: &str, findings: &[(String, String)]) -> String {
    format!(
        "[{name}] SIP RFC 3261 audit violation(s) on the recorded trace — a real \
         UA would have rejected these, so this test MUST fail (RFC check is a \
         mandatory hard gate):\n{}",
        findings
            .iter()
            .map(|(lane, detail)| format!("  • [{lane}] {detail}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

// ---------------------------------------------------------------------------
// Post-mortem trace dump
// ---------------------------------------------------------------------------

/// RAII trace dumper. A failing scenario `panic!`s — a `recv` timeout
/// (`Agent::recv`), an `expect` status mismatch (`expect_response`), a wrong
/// method (`Agent::receive`) — and aborts *before* [`Harness::finish`], the only
/// path that renders the recording. Without this guard the most common failure
/// (a message that never arrived / wrong method) yields a one-line panic and
/// zero visibility into what was actually on the wire.
///
/// This guard's `Drop` notices the in-flight unwind (`std::thread::panicking`)
/// and dumps a compact wire trace to stderr, so every panicking scenario
/// self-documents with no per-test instrumentation. `finish` disarms it (a
/// clean run already has its report). It projects the **synchronous**
/// `channel().snapshot()` — no async, no `close()` — and is best-effort and
/// panic-safe: it never panics inside `Drop` (a poisoned mutex etc. is
/// swallowed).
struct PanicDump {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    recorder: Recorder,
    armed: Cell<bool>,
}

impl PanicDump {
    fn disarm(&self) {
        self.armed.set(false);
    }

    /// Render the compact one-line-per-message trace from the recording.
    fn render(&self) -> String {
        let events = self.channel.snapshot();
        let entries = to_sip_entries(&events);
        let names: BTreeMap<SocketAddr, String> = self
            .recorder
            .snapshot()
            .lanes
            .into_iter()
            .map(|l| (l.addr, l.names.first().cloned().unwrap_or_default()))
            .collect();
        let label_for = |addr: &SocketAddr| match names.get(addr) {
            Some(n) if !n.is_empty() => format!("{n} ({addr})"),
            _ => addr.to_string(),
        };
        let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);

        let mut out = format!(
            "\n══ SIP trace for '{}' (dumped on panic — finish() not reached) ══\n",
            self.name
        );
        if entries.is_empty() {
            out.push_str("  (no messages recorded)\n");
        }
        for e in &entries {
            let sent = format_relative(e.sent_ms as i64 - base);
            let ts = match e.received_ms {
                Some(r) if r != e.sent_ms => format!("{sent} → {}", format_relative(r as i64 - base)),
                _ => sent,
            };
            let undelivered = if e.delivered { "" } else { "  [UNDELIVERED]" };
            out.push_str(&format!(
                "  [{ts}] {} → {}  {}{}\n",
                label_for(&e.from),
                label_for(&e.to),
                facets(&e.raw).label,
                undelivered
            ));
        }
        out.push_str(&format!("══ end SIP trace ({} message(s)) ══\n", entries.len()));
        out
    }
}

impl Drop for PanicDump {
    fn drop(&mut self) {
        if !self.armed.get() || !std::thread::panicking() {
            return;
        }
        // Never panic while already unwinding: a second panic in `Drop` aborts
        // the process. Swallow any failure (e.g. a poisoned mutex on snapshot).
        if let Ok(text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.render())) {
            eprint!("{text}");
        }
    }
}

/// RAII backstop for the RFC 3261 CSeq hard gate when a [`Harness`] is dropped
/// WITHOUT [`Harness::finish`]. `finish` runs the gate inline and disarms this;
/// a harness left to drop (or whose scenario forgot to `finish`) still gets the
/// same mandatory check. On Drop, if still armed and the test is not already
/// unwinding, it computes the cross-message (cseq) findings over the recorded
/// channel and `panic!`s on any — failing the test. Only the cseq rules run here
/// (the structural layer-close anomalies are NOT consulted), so timeout / reap /
/// stall fixtures are not gated. The `!std::thread::panicking()` guard prevents
/// a double-panic when the test is already failing.
struct CseqGate {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    armed: Cell<bool>,
}

impl CseqGate {
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for CseqGate {
    fn drop(&mut self) {
        if !self.armed.get() || std::thread::panicking() {
            return;
        }
        // Reading the snapshot + running the rule is panic-free in practice, but
        // guard it so a render fault can never turn into a double-panic abort.
        let findings = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rfc_cseq_findings(&self.channel.snapshot())
        })) {
            Ok(f) => f,
            Err(_) => return,
        };
        if !findings.is_empty() {
            panic!("{}", render_cseq_panic(&self.name, &findings));
        }
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// A stateful fake UA. Cheap to clone (shares the endpoint + id source); the
/// dialog state lives on the per-transaction handles it returns, not here.
#[derive(Clone)]
pub struct Agent {
    name: String,
    addr: SocketAddr,
    /// Dialog URI (`sip:name@ip`, no port) — used for From/To.
    uri: String,
    ep: Arc<dyn UdpEndpoint>,
    ids: Arc<Ids>,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn branch(&self) -> String {
        format!("z9hG4bK-{}-{}", self.name, self.ids.next())
    }
    fn tag(&self) -> String {
        format!("{}-tag-{}", self.name, self.ids.next())
    }
    fn via(&self) -> ViaSpec {
        ViaSpec {
            local_ip: self.addr.ip().to_string(),
            local_port: self.addr.port(),
            transport: SipTransport::Udp,
            branch: self.branch(),
            custom_params: vec![],
        }
    }
    fn contact(&self) -> ContactSpec {
        ContactSpec {
            user: self.name.clone(),
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            uri_params: vec![],
        }
    }

    async fn send(&self, msg: &SipMessage, dst: SocketAddr) {
        self.ep
            .send_to(&serialize(msg), dst)
            .await
            .unwrap_or_else(|e| panic!("{} send failed: {e}", self.name));
    }

    async fn recv(&self) -> SipMessage {
        let pkt = tokio::time::timeout(RECV_TIMEOUT, self.ep.recv())
            .await
            .unwrap_or_else(|_| panic!("{} timed out waiting for a datagram", self.name))
            .unwrap_or_else(|| panic!("{} endpoint queue closed", self.name));
        CustomParser::new()
            .parse(&pkt.raw)
            .unwrap_or_else(|e| panic!("{} received an unparseable datagram: {e}", self.name))
    }

    /// Begin an out-of-dialog INVITE to `peer`. Returns a builder; call
    /// [`Invite::send`] (optionally after [`Invite::with_sdp`] / [`Invite::through`]).
    pub fn invite<'a>(&'a self, peer: &'a Agent) -> Invite<'a> {
        Invite {
            caller: self,
            peer,
            sdp: None,
            extra_headers: vec![],
            wire_dst: None,
        }
    }

    /// Receive the next request and assert its method. Returns a UAS-side
    /// transaction handle for sending responses.
    pub async fn receive(&self, method: &str) -> ServerTxn {
        match self.recv().await {
            SipMessage::Request(r) => {
                assert!(
                    r.method.eq_ignore_ascii_case(method),
                    "{} expected a {method} request, got {}",
                    self.name,
                    r.method
                );
                // UAS route set (§12.1.1): the request's Record-Route in
                // received order. Used if this UAS later originates in-dialog
                // requests (e.g. bob sends the BYE).
                let route_set = get_headers(&r.headers, "record-route")
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                ServerTxn {
                    agent: self.clone(),
                    request: r,
                    to_tag: None,
                    route_set,
                }
            }
            SipMessage::Response(r) => panic!(
                "{} expected a {method} request, got a {} {} response",
                self.name, r.status, r.reason
            ),
        }
    }

    /// Non-blocking variant of [`receive_tolerating`](Agent::receive_tolerating):
    /// drain (and 200-OK) any *currently queued* `tolerate` requests, and return
    /// `Some(txn)` for the first queued `method` request — or `None` if the queue
    /// is empty (no datagram pending) *without* waiting. Lets a caller poll-advance
    /// the paused clock toward an unknown timer deadline in sub-reap steps: advance
    /// a little, drain, and stop the instant the awaited request appears (CLAUDE.md:
    /// drive between advances; never blow past the deadline + its reap in one step).
    /// Panics only on a *queued* request that is neither `method` nor tolerated.
    pub async fn try_receive_tolerating(
        &self,
        method: &str,
        tolerate: &[&str],
    ) -> Option<ServerTxn> {
        while let Some(pkt) = self.ep.try_recv() {
            let msg = CustomParser::new()
                .parse(&pkt.raw)
                .unwrap_or_else(|e| panic!("{} received an unparseable datagram: {e}", self.name));
            let r = match msg {
                SipMessage::Request(r) => r,
                SipMessage::Response(r) => panic!(
                    "{} expected a {method} request, got a {} {} response",
                    self.name, r.status, r.reason
                ),
            };
            let route_set: Vec<String> = get_headers(&r.headers, "record-route")
                .iter()
                .map(|s| s.to_string())
                .collect();
            let mut txn = ServerTxn {
                agent: self.clone(),
                request: r,
                to_tag: None,
                route_set,
            };
            if txn.request.method.eq_ignore_ascii_case(method) {
                return Some(txn);
            }
            if tolerate.iter().any(|t| t.eq_ignore_ascii_case(&txn.request.method)) {
                txn.respond(200, "OK").send().await;
                continue;
            }
            panic!(
                "{} expected a {method} request (tolerating {tolerate:?}), got {}",
                self.name, txn.request.method
            );
        }
        None
    }

    /// Like [`receive`](Agent::receive), but first drains (and 200-OKs) any
    /// requests whose method is in `tolerate` — the harness equivalent of the
    /// TS `allowExtra(method)`. Under a paused clock an advance that crosses a
    /// timer deadline emits a request whose 2xx round-trip races the txn-layer
    /// retransmit, so several identical copies queue before the awaited message
    /// (CLAUDE.md: tolerate retransmits, don't relax the assertion). Returns the
    /// first request matching `method`.
    pub async fn receive_tolerating(&self, method: &str, tolerate: &[&str]) -> ServerTxn {
        loop {
            let msg = self.recv().await;
            match msg {
                SipMessage::Request(r) => {
                    if r.method.eq_ignore_ascii_case(method) {
                        let route_set = get_headers(&r.headers, "record-route")
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                        return ServerTxn {
                            agent: self.clone(),
                            request: r,
                            to_tag: None,
                            route_set,
                        };
                    }
                    if tolerate.iter().any(|t| t.eq_ignore_ascii_case(&r.method)) {
                        // Drain + answer the duplicate so the txn layer stops
                        // retransmitting it, then keep waiting for `method`.
                        let route_set: Vec<String> = get_headers(&r.headers, "record-route")
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                        let mut txn = ServerTxn {
                            agent: self.clone(),
                            request: r,
                            to_tag: None,
                            route_set,
                        };
                        txn.respond(200, "OK").send().await;
                        continue;
                    }
                    panic!(
                        "{} expected a {method} request (tolerating {tolerate:?}), got {}",
                        self.name, r.method
                    );
                }
                SipMessage::Response(r) => panic!(
                    "{} expected a {method} request, got a {} {} response",
                    self.name, r.status, r.reason
                ),
            }
        }
    }

    /// Send an out-of-dialog REFER addressed to `dst` whose To carries a bogus
    /// tag and whose Request-URI carries a `callRef` the B2BUA never minted — so
    /// the router resolves the (non-existent) call, finds no state, and rejects
    /// it 481 (`maybe_reject_orphan`). Used by the out-of-dialog REFER reject
    /// scenario. Returns a client-transaction handle to `expect` the 481 on.
    pub async fn send_out_of_dialog_refer(
        &self,
        dst: SocketAddr,
        refer_to: &str,
    ) -> InDialogTxn {
        // A synthetic dialog the B2BUA has never seen: fresh Call-ID, a bogus
        // remote (To) tag, and a remote target carrying a bogus stamped callRef
        // (unreserved chars → no escaping needed; the router reads it verbatim),
        // so resolution succeeds but hydration fails → the orphan 481 path.
        let view = StackDialog {
            call_id: format!("orphan-{}-{}", self.name, self.ids.next()),
            local_tag: self.tag(),
            remote_tag: "bogus-refer-tag".into(),
            local_uri: self.uri.clone(),
            remote_uri: format!("sip:unknown@{}", dst.ip()),
            remote_target: format!(
                "sip:unknown@{}:{};callRef=w0-orphan-bogus;leg=b-1",
                dst.ip(),
                dst.port()
            ),
            local_cseq: 0,
            route_set: vec![],
        };
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.via()),
            contact: Some(self.contact()),
            extra_headers: vec![SipHeader {
                name: "Refer-To".into(),
                value: refer_to.into(),
            }],
            ..Default::default()
        };
        let res = generate_in_dialog_request(InDialogMethod::Refer, &view, &opts);
        self.send(&SipMessage::Request(res.request), dst).await;
        InDialogTxn {
            agent: self.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Outgoing INVITE builder + client transaction
// ---------------------------------------------------------------------------

/// Builder for an outgoing INVITE (lets the SDP offer be attached fluently).
pub struct Invite<'a> {
    caller: &'a Agent,
    peer: &'a Agent,
    sdp: Option<String>,
    extra_headers: Vec<SipHeader>,
    /// Wire destination override — the INVITE is *addressed* to `peer` (its
    /// Contact is the Request-URI) but *sent* here. Set by [`Invite::through`]
    /// to route an initial INVITE via a proxy/LB.
    wire_dst: Option<SocketAddr>,
}

impl<'a> Invite<'a> {
    /// Attach an SDP offer body.
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Attach an arbitrary extra header on the initial INVITE (e.g. `Supported:
    /// 100rel, timer` to drive the 18x-management strategies).
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Send the initial INVITE to `proxy` instead of directly to the peer (the
    /// Request-URI still targets the peer). Used to drive an LB/record-routing
    /// proxy; subsequent in-dialog requests then follow the route set learned
    /// from the proxy's Record-Route automatically.
    pub fn through(mut self, proxy: SocketAddr) -> Self {
        self.wire_dst = Some(proxy);
        self
    }

    /// Generate the INVITE (all headers filled in), send it, and return the
    /// client transaction handle.
    pub async fn send(self) -> ClientInvite {
        let caller = self.caller;
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let call_id = format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip());
        let from_tag = caller.tag();
        let request_uri = format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port());

        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: request_uri.clone(),
            call_id: call_id.clone(),
            from_uri: caller.uri.clone(),
            from_tag: from_tag.clone(),
            to_uri: peer.uri.clone(),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            content_type: None,
            extra_headers: self.extra_headers.clone(),
        };
        let invite = generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);
        caller.send(&SipMessage::Request(invite.clone()), wire_dst).await;

        let dialog = StackDialog {
            call_id,
            local_tag: from_tag,
            remote_tag: String::new(),
            local_uri: caller.uri.clone(),
            remote_uri: peer.uri.clone(),
            remote_target: request_uri,
            local_cseq: 1,
            route_set: vec![],
        };
        ClientInvite {
            agent: caller.clone(),
            fallback_addr: peer.addr,
            wire_dst,
            original_invite: invite,
            dialog,
            fork_cseq: HashMap::new(),
        }
    }
}

/// UAC-side INVITE client transaction + the dialog it is establishing.
pub struct ClientInvite {
    agent: Agent,
    /// Where to send the ACK if no Contact was learned (shouldn't happen for a
    /// well-behaved 2xx, but keeps the harness robust).
    fallback_addr: SocketAddr,
    /// The wire destination the INVITE was actually sent to (the proxy/B2BUA when
    /// [`Invite::through`] was used, else the peer). A CANCEL for a still-pending
    /// INVITE must follow the SAME path (RFC 3261 §9.1), so it is retained here.
    wire_dst: SocketAddr,
    original_invite: SipRequest,
    dialog: StackDialog,
    /// Per-forked-early-dialog CSeq (keyed by the fork's To-tag), for the
    /// delayed-offer forking case (RFC 3261 §12.1.2 / §12.2.1.1): one INVITE
    /// creates several early dialogs that each carry an INDEPENDENT CSeq space
    /// seeded from the INVITE's CSeq, so both forks' first PRACKs are
    /// `INVITE_CSeq + 1`. Without this the single shared counter makes each fork's
    /// PRACK (and the later BYE) non-contiguous within its dialog — which the
    /// per-dialog RFC 3261 §12.2.1.1 audit (correctly) rejects. Empty until a
    /// `with_to_tag` request fork is addressed.
    fork_cseq: HashMap<String, u32>,
}

impl ClientInvite {
    /// Wait for and assert a response status. Learns the remote tag (from the
    /// first tagged response) and the remote target (from Contact), so the
    /// later ACK/BYE route and address correctly. Returns the response.
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        let resp = expect_response(&self.agent, status).await;
        if self.dialog.remote_tag.is_empty() {
            if let Some(tag) = &resp.to.tag {
                self.dialog.remote_tag = tag.clone();
            }
        }
        if let Some(target) = first_contact_uri(&resp) {
            self.dialog.remote_target = target;
        }
        // Build the dialog route set from the response's Record-Route, REVERSED
        // (UAC, RFC 3261 §12.1.2), once — so a later 200 doesn't re-seed it.
        if self.dialog.route_set.is_empty() {
            let rr = get_headers(&resp.headers, "record-route");
            if !rr.is_empty() {
                self.dialog.route_set = rr.iter().rev().map(|s| s.to_string()).collect();
            }
        }
        resp
    }

    /// Send a CANCEL for this still-pending INVITE (RFC 3261 §9.1). The CANCEL
    /// reuses the INVITE's Request-URI / Call-ID / From / To / topmost Via branch
    /// and the INVITE's CSeq *number* with method `CANCEL`, and is sent to the
    /// SAME wire destination the INVITE took (the proxy / B2BUA when
    /// [`Invite::through`] was used). Returns a client transaction so the caller
    /// can `expect` the `200 OK` to the CANCEL; the matching `487 Request
    /// Terminated` for the INVITE arrives on this same UA and is consumed via
    /// [`ClientInvite::expect`].
    pub async fn cancel(&self) -> InDialogTxn {
        let cancel = generate_cancel(&InviteClientTransactionHandle {
            original_invite: self.original_invite.clone(),
        });
        self.agent.send(&SipMessage::Request(cancel), self.wire_dst).await;
        InDialogTxn {
            agent: self.agent.clone(),
        }
    }

    /// Begin an in-dialog request on the *early* dialog (before the final 2xx /
    /// ACK) — the PRACK path (RFC 3262): alice PRACKs a reliable 183 while the
    /// INVITE transaction is still pending. The CSeq advances on the shared
    /// dialog state, so the later BYE numbers correctly.
    pub fn send_request(&mut self, method: InDialogMethod) -> InDialogRequest<'_> {
        InDialogRequest::new(self.agent.clone(), &mut self.dialog, self.fallback_addr, method)
            .with_fork_cseq(&mut self.fork_cseq)
    }

    /// Generate and send the ACK for the 2xx (CSeq reused from the INVITE per
    /// RFC 3261 §13.2.2.4), then return the confirmed [`Dialog`]. With a route
    /// set the ACK carries Route headers and goes to the first hop (the proxy).
    pub async fn ack(&mut self) -> Dialog {
        self.ack_with(None).await
    }

    /// ACK the 2xx carrying an optional SDP body — the delayed-offer answer
    /// rides the ACK when the 200 OK carried the offer (RFC 3264 §4).
    pub async fn ack_with(&mut self, sdp: Option<&str>) -> Dialog {
        let handle = InviteClientTransactionHandle {
            original_invite: self.original_invite.clone(),
        };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(Some(&handle), &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
        // If this dialog confirmed to a fork that carried its own PRACK sequence,
        // continue from THAT fork's CSeq so the post-confirm BYE/re-INVITE is
        // contiguous within the winning dialog (RFC 3261 §12.2.1.1), not a reuse
        // of a CSeq the fork already spent.
        let mut confirmed = self.dialog.clone();
        if let Some(&fork) = self.fork_cseq.get(&confirmed.remote_tag) {
            confirmed.local_cseq = confirmed.local_cseq.max(fork);
        }
        Dialog {
            agent: self.agent.clone(),
            fallback_addr: dst,
            dialog: confirmed,
        }
    }
}

// ---------------------------------------------------------------------------
// Confirmed dialog (in-dialog requests)
// ---------------------------------------------------------------------------

/// A confirmed dialog. In-dialog requests auto-increment CSeq and route to the
/// remote target.
pub struct Dialog {
    agent: Agent,
    fallback_addr: SocketAddr,
    dialog: StackDialog,
}

impl Dialog {
    /// Send a BYE (CSeq auto-incremented). Returns its client transaction.
    pub async fn bye(&mut self) -> InDialogTxn {
        self.request(InDialogMethod::Bye, None).await
    }

    /// ACK a re-INVITE's 2xx on this confirmed dialog (RFC 3261 §13.2.2.4 — the
    /// ACK echoes the re-INVITE's CSeq, which `request(INVITE, …)` left as the
    /// dialog's `local_cseq`). Carries an optional SDP answer (the delayed-offer
    /// case where the answer rides the ACK, RFC 3264 §4). Routed to the next hop
    /// like any in-dialog request; the B2BUA relays it end-to-end.
    pub async fn ack(&mut self, sdp: Option<&str>) {
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            cseq: Some(self.dialog.local_cseq),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(None, &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
    }

    /// Send any in-dialog request (re-INVITE, INFO, …); attach an SDP body
    /// with `sdp`.
    pub async fn request(&mut self, method: InDialogMethod, sdp: Option<&str>) -> InDialogTxn {
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.agent.via()),
            contact: Some(self.agent.contact()),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let res = generate_in_dialog_request(method, &self.dialog, &opts);
        self.dialog = res.dialog; // local_cseq bumped
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(res.request), dst).await;
        InDialogTxn {
            agent: self.agent.clone(),
        }
    }

    /// Begin an in-dialog request with fine-grained control over RAck (RFC 3262
    /// PRACK) and arbitrary extra headers. Returns a builder; call
    /// [`InDialogRequest::send`]. Use this over [`request`](Dialog::request) when
    /// the request needs an `RAck` header (PRACK) or other custom headers.
    pub fn send_request(&mut self, method: InDialogMethod) -> InDialogRequest<'_> {
        InDialogRequest::new(self.agent.clone(), &mut self.dialog, self.fallback_addr, method)
    }
}

/// Builder for an in-dialog request carrying an `RAck` and/or custom headers
/// (the PRACK path, RFC 3262 §7.2). Borrows the originating dialog's
/// [`StackDialog`] so the CSeq bump persists — works over both a confirmed
/// [`Dialog`] and an early [`ClientInvite`] (PRACK precedes the final 2xx/ACK).
pub struct InDialogRequest<'a> {
    agent: Agent,
    dialog: &'a mut StackDialog,
    fallback: SocketAddr,
    method: InDialogMethod,
    sdp: Option<String>,
    rack: Option<String>,
    extra_headers: Vec<SipHeader>,
    to_tag: Option<String>,
    /// Per-fork CSeq map (see [`ClientInvite::fork_cseq`]). Present only on the
    /// early-dialog path; when a `with_to_tag` fork is addressed the CSeq comes
    /// from this independent per-fork sequence, not the shared dialog counter.
    fork_cseq: Option<&'a mut HashMap<String, u32>>,
}

impl<'a> InDialogRequest<'a> {
    fn new(agent: Agent, dialog: &'a mut StackDialog, fallback: SocketAddr, method: InDialogMethod) -> Self {
        InDialogRequest {
            agent,
            dialog,
            fallback,
            method,
            sdp: None,
            rack: None,
            extra_headers: vec![],
            to_tag: None,
            fork_cseq: None,
        }
    }

    /// Wire in the originating [`ClientInvite`]'s per-fork CSeq map so a
    /// `with_to_tag` request uses that fork's independent sequence.
    fn with_fork_cseq(mut self, map: &'a mut HashMap<String, u32>) -> Self {
        self.fork_cseq = Some(map);
        self
    }

    /// Address this request to a specific early dialog by overriding the remote
    /// (To) tag — the per-fork PRACK in `prack-forking` (RFC 3262 §5). The shared
    /// CSeq counter still advances; the slim harness does not assert per-fork CSeq
    /// independence (the B2BUA recomputes the outbound CSeq per dialog anyway).
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// Attach an SDP body (e.g. the answer carried in a PRACK to a delayed
    /// offer, RFC 3264 §4).
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Set the `RAck` header (`<rseq> <cseq> <method>`, RFC 3262 §7.2).
    pub fn with_rack(mut self, rack: &str) -> Self {
        self.rack = Some(rack.to_string());
        self
    }

    /// Attach an arbitrary extra header.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Generate and send the request; returns its client transaction.
    pub async fn send(mut self) -> InDialogTxn {
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.agent.via()),
            contact: Some(self.agent.contact()),
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            rack: self.rack,
            extra_headers: self.extra_headers,
            ..Default::default()
        };
        // Per-fork addressing: generate against a dialog view with the chosen
        // remote tag. For a forked early dialog (a `with_to_tag` other than the
        // shared dialog's own tag) the CSeq rides that fork's OWN sequence
        // (seeded from the INVITE's CSeq), not the shared counter — RFC 3261
        // §12.2.1.1: each early dialog increments independently.
        let mut view = self.dialog.clone();
        let mut opts = opts;
        // On the early-dialog path EVERY explicit `with_to_tag` addresses a
        // distinct forked early dialog (RFC 3262 §5), so it rides that fork's OWN
        // CSeq sequence (seeded from the INVITE's CSeq) — NOT the shared counter,
        // which a sibling fork's PRACK must not perturb.
        let forked = self.to_tag.is_some() && self.fork_cseq.is_some();
        if let (Some(tag), Some(map)) = (self.to_tag.as_ref(), self.fork_cseq.as_deref_mut()) {
            view.remote_tag = tag.clone();
            // Seed from the INVITE's CSeq (the dialog's current local_cseq) the
            // first time this fork is addressed, then advance by one per request.
            let entry = map.entry(tag.clone()).or_insert(self.dialog.local_cseq);
            *entry += 1;
            opts.cseq = Some(*entry);
        } else if let Some(t) = &self.to_tag {
            view.remote_tag = t.clone();
        }
        let res = generate_in_dialog_request(self.method, &view, &opts);
        // Advance the SHARED dialog counter only for a non-forked request (a
        // forked request advanced its own per-fork entry above and must leave the
        // shared sequence untouched).
        if !forked {
            self.dialog.local_cseq = res.dialog.local_cseq;
        }
        let dst = next_hop(self.dialog, self.fallback);
        self.agent.send(&SipMessage::Request(res.request), dst).await;
        InDialogTxn {
            agent: self.agent.clone(),
        }
    }
}

/// Client transaction for an in-dialog request.
pub struct InDialogTxn {
    agent: Agent,
}

impl InDialogTxn {
    /// Wait for and assert a response status.
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        expect_response(&self.agent, status).await
    }

    /// Like [`expect`](InDialogTxn::expect), but first drains (and 200-OKs) any
    /// inbound requests whose method is in `tolerate` — the response-side analog
    /// of [`Agent::receive_tolerating`]. Under a paused clock a keepalive OPTIONS
    /// retransmit can race the awaited response on the same socket; tolerate it
    /// rather than relax the assertion (CLAUDE.md retransmit hazard).
    pub async fn expect_tolerating(&mut self, status: u16, tolerate: &[&str]) -> SipResponse {
        expect_response_tolerating(&self.agent, status, tolerate).await
    }
}

// ---------------------------------------------------------------------------
// UAS-side server transaction
// ---------------------------------------------------------------------------

/// UAS-side transaction for a received request. `respond` echoes Via/From/To/
/// Call-ID/CSeq and mints a stable To-tag on the first non-100 response.
pub struct ServerTxn {
    agent: Agent,
    request: SipRequest,
    to_tag: Option<String>,
    route_set: Vec<String>,
}

impl ServerTxn {
    /// The received request (for inspecting headers / SDP).
    pub fn request(&self) -> &SipRequest {
        &self.request
    }

    /// Send a response. Returns a builder for attaching an SDP answer and/or
    /// custom headers (e.g. `Require: 100rel` + `RSeq` on a reliable 18x).
    pub fn respond(&mut self, status: u16, reason: &str) -> Respond<'_> {
        Respond {
            txn: self,
            status,
            reason: reason.to_string(),
            sdp: None,
            extra_headers: vec![],
            to_tag: None,
        }
    }

    /// Form the UAS-side confirmed [`Dialog`] for this transaction, so this UA
    /// can originate in-dialog requests (e.g. the callee sends the BYE). Call
    /// after responding 2xx (so the To-tag is minted). The remote target is the
    /// caller's Contact; the route set is the request's Record-Route in order
    /// (§12.1.1), so in-dialog requests route back through any proxy.
    pub fn dialog(&self) -> Dialog {
        let req = &self.request;
        let local_tag = self.to_tag.clone().unwrap_or_default();
        let remote_target = get_header(&req.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_else(|| req.from.uri.clone());
        let dialog = StackDialog {
            call_id: req.call_id.clone(),
            local_tag,
            remote_tag: req.from.tag.clone().unwrap_or_default(),
            // From the UAS's view, "local" is itself and "remote" is the caller.
            local_uri: self.agent.uri.clone(),
            remote_uri: req.from.uri.clone(),
            remote_target,
            local_cseq: 0, // UAS originates its own CSeq space; first request → 1
            route_set: self.route_set.clone(),
        };
        let fallback = next_hop(&dialog, top_via_addr(req).unwrap_or(self.agent.addr));
        Dialog {
            agent: self.agent.clone(),
            fallback_addr: fallback,
            dialog,
        }
    }
}

/// Builder for a UAS response (lets an SDP answer and custom headers be
/// attached fluently).
pub struct Respond<'a> {
    txn: &'a mut ServerTxn,
    status: u16,
    reason: String,
    sdp: Option<String>,
    extra_headers: Vec<SipHeader>,
    to_tag: Option<String>,
}

impl<'a> Respond<'a> {
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Force a specific To-tag on this response instead of the auto-minted one.
    /// Used to simulate a forking endpoint emitting several early dialogs with
    /// distinct tags (RFC 3261 §12.1; the per-fork 18x in `prack-forking`).
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// Attach a custom header (e.g. `Require: 100rel`, `RSeq: 1` on a reliable
    /// provisional, RFC 3262). Repeatable; order is preserved.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Generate and send the response.
    pub async fn send(self) {
        let txn = self.txn;
        // An explicit per-fork To-tag overrides (and does not disturb) the txn's
        // sticky auto-minted tag, so distinct early dialogs keep distinct tags.
        let to_tag = if let Some(t) = self.to_tag {
            Some(t)
        } else {
            if self.status > 100 && txn.to_tag.is_none() {
                txn.to_tag = Some(txn.agent.tag());
            }
            txn.to_tag.clone()
        };
        // Contact is required on 2xx and useful on 18x to establish the early
        // dialog's remote target; omit on plain 100.
        let contact = if self.status >= 180 {
            Some(txn.agent.contact())
        } else {
            None
        };
        let opts = GenerateResponseOpts {
            to_tag,
            contact,
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            content_type: None,
            extra_headers: self.extra_headers.clone(),
            incoming_source: None,
        };
        let resp = generate_response(&txn.request, self.status, &self.reason, &opts);
        // Responses are routed by Via, not Route (RFC 3261 §18.2.2): send to the
        // request's topmost Via sent-by. With a proxy in the path that Via is
        // the proxy's, so the response correctly traverses it back.
        let dst = top_via_addr(&txn.request).unwrap_or(txn.agent.addr);
        txn.agent.send(&SipMessage::Response(resp), dst).await;
    }
}

/// Allow `respond(...).await` directly (no explicit `.send()`), by making the
/// builder awaitable.
impl<'a> std::future::IntoFuture for Respond<'a> {
    type Output = ();
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}

// ---------------------------------------------------------------------------
// Proxy / load balancer (loose-routing, Record-Route)
// ---------------------------------------------------------------------------

/// A minimal loose-routing proxy — the test stand-in for the LB front proxy
/// (port target: `sip-front-proxy`). It does the load-bearing routing surgery
/// per RFC 3261 §16:
///   - adds its own **Via** (top) to forwarded requests so responses route back
///     through it (§16.6), and strips that Via from responses (§16.7);
///   - inserts a `;lr` **Record-Route** (top) on dialog-creating INVITEs so both
///     peers route in-dialog requests through it (§16.6.4);
///   - strips its own top **Route** from in-dialog requests it is the loose
///     router for (§16.4) before forwarding.
///
/// It is *stateless* and *scripted*: the test says which way to forward each
/// message (the real proxy resolves the next hop from the top Route / RURI).
#[derive(Clone)]
pub struct Proxy {
    agent: Agent,
}

impl Proxy {
    pub fn addr(&self) -> SocketAddr {
        self.agent.addr
    }
    pub fn name(&self) -> &str {
        &self.agent.name
    }

    fn record_route_value(&self) -> String {
        format!("<sip:{}:{};lr>", self.agent.addr.ip(), self.agent.addr.port())
    }

    /// Receive one request, apply the §16 surgery, and forward it to `next`.
    /// Returns the (rewritten) request for assertions.
    pub async fn forward_request(&self, next: SocketAddr) -> SipRequest {
        let SipMessage::Request(mut req) = self.agent.recv().await else {
            panic!("{} expected a request to forward", self.agent.name);
        };
        // Loose router popping itself off the route set (§16.4) — in-dialog
        // requests (ACK/BYE/…) arrive with our Record-Route as the top Route.
        strip_top_route_if_self(&mut req, self.agent.addr);
        // Record-Route dialog-creating requests so in-dialog traffic returns
        // through us (§16.6.4). A dialog-creating INVITE has no To-tag yet.
        if req.method.eq_ignore_ascii_case("INVITE") && req.to.tag.is_none() {
            prepend_header(&mut req.headers, "Record-Route", &self.record_route_value());
        }
        // Add our Via on top so the response comes back to us (§16.6).
        prepend_header(&mut req.headers, "Via", &self.via_value());
        self.agent.send(&SipMessage::Request(req.clone()), next).await;
        req
    }

    /// Receive one response, strip our Via, and forward it to `next`.
    pub async fn forward_response(&self, next: SocketAddr) -> SipResponse {
        let SipMessage::Response(mut resp) = self.agent.recv().await else {
            panic!("{} expected a response to forward", self.agent.name);
        };
        strip_top_via_if_self(&mut resp.headers, self.agent.addr);
        self.agent.send(&SipMessage::Response(resp.clone()), next).await;
        resp
    }

    fn via_value(&self) -> String {
        format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.agent.addr.ip(),
            self.agent.addr.port(),
            self.agent.branch()
        )
    }
}

/// Insert a header at the top of the list (RFC 3261 §16.6 prepend semantics for
/// Via / Record-Route).
fn prepend_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    headers.insert(
        0,
        SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        },
    );
}

/// Strip the first Route header if it routes to `me` (the loose router removing
/// itself, §16.4).
fn strip_top_route_if_self(req: &mut SipRequest, me: SocketAddr) {
    if let Some(pos) = req
        .headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("route"))
    {
        let uri = strip_route_uri_to_request_uri(&req.headers[pos].value);
        if uri_to_addr(&uri) == Some(me) {
            req.headers.remove(pos);
        }
    }
}

/// Strip the topmost Via if it is `me`'s (the proxy removing its own Via from a
/// response before forwarding upstream, §16.7).
fn strip_top_via_if_self(headers: &mut Vec<SipHeader>, me: SocketAddr) {
    if let Some(pos) = headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("via"))
    {
        let sent_by = headers[pos]
            .value
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.split(';').next())
            .map(str::trim);
        if let Some(addr) = sent_by.and_then(hostport_to_addr) {
            if addr == me {
                headers.remove(pos);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn expect_response(agent: &Agent, status: u16) -> SipResponse {
    loop {
        match agent.recv().await {
            // A real UAC absorbs an unsolicited 100 Trying (RFC 3261 §8.1.3.2) —
            // a stateful upstream (B2BUA / proxy txn layer) emits it before the
            // first real provisional. Skip it unless 100 is what we await.
            SipMessage::Response(r) if r.status == 100 && status != 100 => continue,
            SipMessage::Response(r) => {
                assert_eq!(
                    r.status, status,
                    "{} expected a {status} response, got {} {}",
                    agent.name, r.status, r.reason
                );
                return r;
            }
            SipMessage::Request(r) => panic!(
                "{} expected a {status} response, got a {} request",
                agent.name, r.method
            ),
        }
    }
}

/// [`expect_response`] that drains + 200-OKs `tolerate`d inbound requests
/// (e.g. keepalive OPTIONS retransmits) racing the awaited response.
async fn expect_response_tolerating(agent: &Agent, status: u16, tolerate: &[&str]) -> SipResponse {
    loop {
        match agent.recv().await {
            SipMessage::Response(r) if r.status == 100 && status != 100 => continue,
            SipMessage::Response(r) => {
                assert_eq!(
                    r.status, status,
                    "{} expected a {status} response, got {} {}",
                    agent.name, r.status, r.reason
                );
                return r;
            }
            SipMessage::Request(r) if tolerate.iter().any(|t| t.eq_ignore_ascii_case(&r.method)) => {
                let route_set: Vec<String> = get_headers(&r.headers, "record-route")
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                let mut txn = ServerTxn {
                    agent: agent.clone(),
                    request: r,
                    to_tag: None,
                    route_set,
                };
                txn.respond(200, "OK").send().await;
                continue;
            }
            SipMessage::Request(r) => panic!(
                "{} expected a {status} response (tolerating {tolerate:?}), got a {} request",
                agent.name, r.method
            ),
        }
    }
}

/// Unwrap a `<uri>` name-addr / Route value to its bare URI (params after `>`
/// dropped); a bare URI passes through trimmed.
fn unwrap_angle(value: &str) -> String {
    let t = value.trim();
    match (t.find('<'), t.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
        _ => t.to_string(),
    }
}

/// The first Contact URI on a response, unwrapped from `<...>`. Used to learn
/// the dialog remote target.
fn first_contact_uri(resp: &SipResponse) -> Option<String> {
    get_header(&resp.headers, "contact").map(unwrap_angle)
}

/// Resolve a SIP URI to a socket address (default port 5060, IPv4 fixtures
/// only). Handles `sip:user@host:port`, the userless `sip:host:port;lr` form
/// of a Route/Record-Route URI, and a bare `host:port`.
fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let no_scheme = uri
        .strip_prefix("sips:")
        .or_else(|| uri.strip_prefix("sip:"))
        .unwrap_or(uri);
    // Host part is whatever follows the last '@' (none → the whole thing).
    let host_part = no_scheme.rsplit('@').next()?;
    let host_port = host_part.split([';', '?']).next()?.trim();
    hostport_to_addr(host_port)
}

/// Parse a bare `host:port` (or `host`, default port 5060) to a socket address.
fn hostport_to_addr(host_port: &str) -> Option<SocketAddr> {
    if let Ok(sa) = host_port.parse::<SocketAddr>() {
        return Some(sa);
    }
    format!("{host_port}:5060").parse().ok()
}

/// The wire destination for an in-dialog request: the first hop in the route
/// set (the proxy) when present, else the dialog's remote target. For both
/// loose and strict routing the next hop is the address of `route_set[0]`'s
/// URI; with no route set it is the remote target.
fn next_hop(dialog: &StackDialog, fallback: SocketAddr) -> SocketAddr {
    if let Some(top) = dialog.route_set.first() {
        if let Some(addr) = uri_to_addr(&strip_route_uri_to_request_uri(top)) {
            return addr;
        }
    }
    uri_to_addr(&dialog.remote_target).unwrap_or(fallback)
}

/// The address a response to `req` must be sent to: the topmost Via's sent-by
/// (RFC 3261 §18.2.2). (`received=`/`rport=` are not stamped by this harness's
/// `generate_response`, so the sent-by host:port is authoritative here.)
fn top_via_addr(req: &SipRequest) -> Option<SocketAddr> {
    let via = get_header(&req.headers, "via")?;
    // "SIP/2.0/UDP host:port;branch=…" → take the token after the transport,
    // before the first ';'.
    let after_transport = via.split_whitespace().nth(1)?;
    let sent_by = after_transport.split(';').next()?.trim();
    hostport_to_addr(sent_by)
}
