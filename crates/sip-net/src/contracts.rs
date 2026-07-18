//! **TEST-ONLY.** Contract decorators for `SignalingNetwork` — port of
//! `SignalingNetwork.contracts.ts`. The worked example the `effect-layer-test`
//! SKILL points at, reshaped from Effect `Layer` wrappers to **decorator
//! structs that implement the same trait**.
//!
//! Two decorators, composed in the canonical order
//! `paranoidInputs(scopedAudit(impl))` (the source's `propertyTest` is skipped
//! for `SignalingNetwork` — `bind_udp` opens a socket and `send_to` is
//! fire-and-forget UDP, so there is no natural input/output domain to assert a
//! per-call property over):
//!
//!   - [`RecordingSignalingNetwork`] — the `scopedAudit` equivalent. Records
//!     every method call onto the typed `layer-harness` channel and runs
//!     per-bind + cross-message RFC rules, surfacing findings on the shared
//!     anomaly ledger at the severity the active [`RunContext`] dictates.
//!   - [`ParanoidSignalingNetwork`] — the `paranoidInputs` equivalent.
//!     Caller-side precondition checks; a violation is a programmer error
//!     (defect) raised by panic, the Rust analogue of the source's
//!     `Effect.die`.
//!
//! Production composes the bare `Real`/`Simulated` impl directly. These
//! decorators require a `Recorder` + a `RunContext`, which production does not
//! provide — never compose them into a production network tree.
//!
//! Scope-close semantics. The source ran rules in Effect scope finalizers
//! (per-bind close, then layer close). Rust uses RAII for the per-bind close
//! (an endpoint's [`Drop`] records its release, checks for a queue leak, and
//! runs its peer rules) and an explicit [`RecordingSignalingNetwork::close`]
//! for the layer close (drain transit, structural invariants, cross-message
//! rules, and the deferred-fail → violation decision). **Drop your endpoints
//! before calling `close()`** so their per-bind findings are on the ledger
//! when `close()` reads it — the RAII analogue of LIFO scope finalizers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use layer_harness::recording::{record_call, CallOutcome};
use layer_harness::time::now_ms;
use layer_harness::{lane_key, Channel, LaneKey, RecordedAnomaly, Recorder, RunContext, Severity, Stamped};

use crate::net::{SignalingNetwork, UdpEndpoint};
use crate::types::{
    all_ua_roles, BindError, BindSummary, BindUdpOpts, RecvDisposition, SendError, UaRole,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

/// The `layer-harness` channel key for this layer. The `RunContext` uses the
/// same string to decide whether a `unit-test-of-layer` targets us.
pub const SIGNALING_TAG: &str = "sip-net/SignalingNetwork";

// ---------------------------------------------------------------------------
// Typed event union
// ---------------------------------------------------------------------------

/// One observation on the `SignalingNetwork` typed channel. Every variant
/// carries the lane-identifying `bind_key` so per-peer rules can slice on a
/// single peer. (Port of `SignalingNetworkEvent`.)
///
/// `RecvItem` is the ARRIVAL fact, recorded at delivery into the endpoint's
/// inbox (via the [`crate::types::RecvTap`] the decorator installs at bind);
/// `RecvConsumed` is the CONSUMPTION fact, recorded when the endpoint's
/// `recv`/`try_recv` returns the packet. A `RecvItem` with no matching
/// `RecvConsumed` is a message that really crossed the wire but the scenario
/// body never read (newkahneed-036 ask A) — still on the ladder, still seen by
/// the RFC audit. On an impl with no tappable inbox the decorator falls back to
/// recording both, adjacently, at recv time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalingNetworkEvent {
    BindAcquire { bind_key: LaneKey, summary: BindSummary },
    BindRelease { bind_key: LaneKey },
    SendCalled { bind_key: LaneKey, to: SocketAddr, msg: Vec<u8> },
    SendResult { bind_key: LaneKey, outcome: SendOutcome },
    RecvItem { bind_key: LaneKey, packet: UdpPacket, disposition: RecvDisposition },
    RecvConsumed { bind_key: LaneKey, packet: UdpPacket },
    /// An outbound datagram the endpoint's retransmit engine re-emitted below
    /// the recording layer (loadgen `--auto-retransmit`). Projection-only — it
    /// renders on the ladder as a tagged outbound frame but is invisible to the
    /// RFC audit (a re-emit is a byte-identical retransmission the rules dedup).
    ReEmit { bind_key: LaneKey, to: SocketAddr, msg: Vec<u8>, kind: crate::types::ReEmitKind },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Ok,
    Err,
}

impl SignalingNetworkEvent {
    pub fn bind_key(&self) -> &LaneKey {
        match self {
            SignalingNetworkEvent::BindAcquire { bind_key, .. }
            | SignalingNetworkEvent::BindRelease { bind_key }
            | SignalingNetworkEvent::SendCalled { bind_key, .. }
            | SignalingNetworkEvent::SendResult { bind_key, .. }
            | SignalingNetworkEvent::RecvItem { bind_key, .. }
            | SignalingNetworkEvent::RecvConsumed { bind_key, .. }
            | SignalingNetworkEvent::ReEmit { bind_key, .. } => bind_key,
        }
    }
}

/// Whether the RFC audit sees this event. `RecvConsumed` is a projection-only
/// consumption marker; a `RecvItem` is included per its disposition's
/// [`RecvDisposition::audit_visible`] (modeled loss / infra-absorbed duplicates
/// are deliberately invisible to the rules). EVERY rule invocation — the
/// per-bind Drop pass, the layer-close cross pass, and any external harness
/// running `rfc_*_rules` over a channel snapshot — must filter through this, so
/// the rules keep one view of the wire.
pub fn audit_visible_event(event: &SignalingNetworkEvent) -> bool {
    match event {
        SignalingNetworkEvent::RecvConsumed { .. } => false,
        // A re-emit is a byte-identical retransmission — projection-only, so the
        // rules keep one un-duplicated view of the wire.
        SignalingNetworkEvent::ReEmit { .. } => false,
        SignalingNetworkEvent::RecvItem { disposition, .. } => disposition.audit_visible(),
        _ => true,
    }
}

/// The audit anomaly kinds this layer owns; the layer-close failure decision
/// fails on any of these whose severity is non-advisory.
fn is_audit_kind(kind: &str) -> bool {
    matches!(
        kind,
        "signalingAudit" | "queueLeak" | "undeliverable" | "inFlightImbalance"
    )
}

// ---------------------------------------------------------------------------
// Failure shape
// ---------------------------------------------------------------------------

/// Surfaced by [`RecordingSignalingNetwork::close`] when a non-advisory audit
/// finding is on the ledger. (Port of `SignalingAuditViolation`.)
#[derive(Debug, Clone, thiserror::Error)]
#[error("signaling audit [{check}]: {detail}")]
pub struct SignalingAuditViolation {
    pub check: String,
    pub detail: String,
    pub bind_key: Option<LaneKey>,
}

// ---------------------------------------------------------------------------
// Rule interfaces
// ---------------------------------------------------------------------------

/// A per-peer rule. Sees the events captured for a single `bind_key` and
/// returns zero or more violation detail strings. `subject` is the SIP role(s)
/// it covers; it runs on a bind only when `subject` intersects the bind's
/// declared roles. (Port of `PeerAuditRule`.)
pub trait PeerAuditRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn subject(&self) -> HashSet<UaRole> {
        all_ua_roles()
    }
    fn force_advisory(&self) -> bool {
        false
    }
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], bind_key: &str) -> Vec<String>;
}

/// A cross-message rule operates on the full event log at layer close. Each
/// finding carries an originating `bind_key`. (Port of `CrossMessageAuditRule`.)
pub trait CrossMessageAuditRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn subject(&self) -> HashSet<UaRole> {
        all_ua_roles()
    }
    fn force_advisory(&self) -> bool {
        false
    }
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)>;

    /// The findings with the 1-based wire-entry index of the OFFENDING message
    /// (into `to_sip_entries(events)`), when the rule can pinpoint it. The
    /// default derives from [`check`](Self::check) with no index; a rule that
    /// knows the exact entry overrides THIS instead. Consumers scope waivers on
    /// the index; a `None` finding is unattributable to a party/position.
    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>)> {
        self.check(events).into_iter().map(|(b, d)| (b, d, None)).collect()
    }
}

use std::collections::HashSet;

/// Predicate: should this bind be audited at all? Returning `false`
/// short-circuits rule evaluation for the peer (events are still recorded).
/// The source's `shouldAuditBind` escape valve.
pub type ShouldAuditBind = Arc<dyn Fn(&LaneKey) -> bool + Send + Sync>;

/// Options for the recording/audit decorator. (Port of `ScopedAuditOptions`;
/// the per-test `exceptions` ledger is deferred — see MIGRATION_STATUS.)
#[derive(Default, Clone)]
pub struct ScopedAuditOptions {
    pub rules: Vec<Arc<dyn PeerAuditRule>>,
    pub cross_message_rules: Vec<Arc<dyn CrossMessageAuditRule>>,
    pub should_audit_bind: Option<ShouldAuditBind>,
}

fn subject_intersects(subject: &HashSet<UaRole>, roles: &HashSet<UaRole>) -> bool {
    subject.iter().any(|r| roles.contains(r))
}

// ---------------------------------------------------------------------------
// RecordingSignalingNetwork (scopedAudit equivalent)
// ---------------------------------------------------------------------------

struct RecordingInner {
    inner: Arc<dyn SignalingNetwork>,
    recorder: Recorder,
    ctx: RunContext,
    channel: Channel<SignalingNetworkEvent>,
    rules: Arc<Vec<Arc<dyn PeerAuditRule>>>,
    cross_rules: Arc<Vec<Arc<dyn CrossMessageAuditRule>>>,
    should_audit: ShouldAuditBind,
    bind_roles: Mutex<HashMap<LaneKey, HashSet<UaRole>>>,
    closed: AtomicBool,
}

/// Records every call onto the typed channel and runs the audit rules. Clone
/// shares the same recorder + channel, so a clone kept for `close()` sees the
/// same ledger the wrapped network wrote to.
#[derive(Clone)]
pub struct RecordingSignalingNetwork(Arc<RecordingInner>);

impl RecordingSignalingNetwork {
    pub fn new(
        inner: Arc<dyn SignalingNetwork>,
        recorder: Recorder,
        ctx: RunContext,
        opts: ScopedAuditOptions,
    ) -> Self {
        let channel = recorder.for_tag::<SignalingNetworkEvent>(SIGNALING_TAG);
        let should_audit = opts
            .should_audit_bind
            .unwrap_or_else(|| Arc::new(|_: &LaneKey| true));
        Self(Arc::new(RecordingInner {
            inner,
            recorder,
            ctx,
            channel,
            rules: Arc::new(opts.rules),
            cross_rules: Arc::new(opts.cross_message_rules),
            should_audit,
            bind_roles: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        }))
    }

    /// The recorder this decorator writes to — for snapshot/anomaly assertions.
    pub fn recorder(&self) -> Recorder {
        self.0.recorder.clone()
    }

    /// The typed channel, for tests/projectors that read the raw event log
    /// (e.g. a SIP-wire derivation).
    pub fn channel(&self) -> Channel<SignalingNetworkEvent> {
        self.0.channel.clone()
    }

    /// Layer-close finalizer. Drains in-memory transit, runs the structural
    /// invariants (in-flight balance, undeliverable packets, residual queue
    /// leaks — simulated impl only) and the cross-message rules, then fails
    /// with the first non-advisory finding if the active `RunContext` is not
    /// `real-run`. Idempotent.
    pub async fn close(&self) -> Result<(), SignalingAuditViolation> {
        if self.0.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let inner = &self.0.inner;

        // Structural checks only make sense for the in-memory fabric; the real
        // impl reports `transit_delay_ms() == None` and stubs the accessors.
        if inner.transit_delay_ms().is_some() {
            inner.await_in_flight(Duration::from_millis(200)).await;

            let in_flight = inner.in_flight();
            if in_flight != 0 {
                self.push_layer_anomaly(
                    "inFlightImbalance",
                    "A1_inFlightImbalance",
                    format!("{in_flight} transit fibers still in flight at layer close"),
                    None,
                );
            }

            for pkt in inner.drain_undeliverable().await {
                self.push_layer_anomaly(
                    "undeliverable",
                    "A2_undeliverable",
                    format!("{} → {} never delivered (no endpoint bound)", pkt.src, pkt.dst),
                    Some(lane_key(pkt.dst)),
                );
            }

            for (addr, depth) in inner.queue_depths() {
                if depth == 0 {
                    continue;
                }
                self.push_layer_anomaly(
                    "queueLeak",
                    "A3_queueLeak",
                    format!("{depth} packets queued at layer close"),
                    Some(lane_key(addr)),
                );
            }
        }

        // Cross-message rules — one pass over the whole channel (audit view:
        // consumption markers + modeled-loss arrivals filtered out).
        if !self.0.cross_rules.is_empty() {
            let snapshot: Vec<Stamped<SignalingNetworkEvent>> = self
                .0
                .channel
                .snapshot()
                .into_iter()
                .filter(|s| audit_visible_event(&s.event))
                .collect();
            let bind_roles = self.0.bind_roles.lock().unwrap().clone();
            for rule in self.0.cross_rules.iter() {
                for (bind_key, detail) in rule.check(&snapshot) {
                    if !(self.0.should_audit)(&bind_key) {
                        continue;
                    }
                    let roles = bind_roles.get(&bind_key).cloned().unwrap_or_else(all_ua_roles);
                    if !subject_intersects(&rule.subject(), &roles) {
                        continue;
                    }
                    let severity = if rule.force_advisory() {
                        Severity::Advisory
                    } else {
                        self.0.ctx.severity_for(SIGNALING_TAG, false)
                    };
                    self.push_anomaly("signalingAudit", rule.name(), detail, severity, Some(bind_key));
                }
            }
        }

        // Fail with the first non-advisory audit finding (per-bind findings
        // were pushed by endpoint Drop; layer + cross-message findings just
        // now). `real-run` never fails.
        if self.0.ctx.rules_enabled() {
            for a in self.0.recorder.anomalies() {
                if is_audit_kind(a.kind) && a.severity.fails() {
                    return Err(SignalingAuditViolation {
                        check: a.check,
                        detail: a.detail,
                        bind_key: a.bind_key,
                    });
                }
            }
        }
        Ok(())
    }

    fn push_layer_anomaly(
        &self,
        kind: &'static str,
        check: &'static str,
        detail: String,
        bind_key: Option<LaneKey>,
    ) {
        // Layer-close structural findings are deferred-fail in a recorder test
        // (advisory in real-run), matching the source's tier for these.
        let severity = self.0.ctx.severity_for(SIGNALING_TAG, false);
        self.push_anomaly(kind, check, detail, severity, bind_key);
    }

    fn push_anomaly(
        &self,
        kind: &'static str,
        check: impl Into<String>,
        detail: impl Into<String>,
        severity: Severity,
        bind_key: Option<LaneKey>,
    ) {
        let seq = self.0.recorder.sequencer().next();
        self.0.recorder.record_anomaly(RecordedAnomaly::new(
            kind,
            check,
            detail,
            severity,
            bind_key,
            seq,
            now_ms(),
        ));
    }
}

#[async_trait]
impl SignalingNetwork for RecordingSignalingNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        // Logical sub-lane (036 ask C): a labelled bind records under
        // `ip:port#<label>` so several logical endpoints sharing one socket
        // stay distinct columns. `report::lane_addr` recovers the address.
        let bind_key = match &opts.lane_label {
            Some(label) => format!("{}#{label}", lane_key(opts.addr)),
            None => lane_key(opts.addr),
        };
        let roles = opts.effective_roles();
        self.0
            .bind_roles
            .lock()
            .unwrap()
            .insert(bind_key.clone(), roles.clone());
        let summary = opts.summary();
        self.0.channel.record(SignalingNetworkEvent::BindAcquire {
            bind_key: bind_key.clone(),
            summary,
        });
        let endpoint = self.0.inner.bind_udp(opts).await?;
        // Delivery-time recording (newkahneed-036 ask A): arrivals are recorded
        // the moment the inner inbox accepts (or overflows/refuses) them, so a
        // packet the body never reads is still on the trace. Falls back to
        // recv-time recording on impls with no tappable inbox.
        let tap_channel = self.0.channel.clone();
        let tap_key = bind_key.clone();
        let tapped = endpoint.install_recv_tap(Arc::new(move |pkt, disposition| {
            tap_channel.record(SignalingNetworkEvent::RecvItem {
                bind_key: tap_key.clone(),
                packet: pkt.clone(),
                disposition,
            });
        }));
        // Send-side twin (loadgen mux only): re-emitted recovery frames go on the
        // wire below this decorator, so tap them into the channel as `ReEmit`
        // events — visible on the ladder, invisible to the audit.
        let sendtap_channel = self.0.channel.clone();
        let sendtap_key = bind_key.clone();
        endpoint.install_send_tap(Arc::new(move |raw: &[u8], to, kind| {
            sendtap_channel.record(SignalingNetworkEvent::ReEmit {
                bind_key: sendtap_key.clone(),
                to,
                msg: raw.to_vec(),
                kind,
            });
        }));
        Ok(Box::new(RecordedEndpoint {
            inner: endpoint,
            channel: self.0.channel.clone(),
            recorder: self.0.recorder.clone(),
            ctx: self.0.ctx,
            bind_key,
            roles,
            rules: self.0.rules.clone(),
            should_audit: self.0.should_audit.clone(),
            tapped,
        }))
    }

    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket> {
        self.0.inner.drain_undeliverable().await
    }
    fn transit_delay_ms(&self) -> Option<u64> {
        self.0.inner.transit_delay_ms()
    }
    fn in_flight(&self) -> i64 {
        self.0.inner.in_flight()
    }
    fn bump_in_flight(&self, delta: i64) {
        self.0.inner.bump_in_flight(delta)
    }
    fn queue_depths(&self) -> Vec<(SocketAddr, usize)> {
        self.0.inner.queue_depths()
    }
    async fn await_in_flight(&self, timeout: Duration) {
        self.0.inner.await_in_flight(timeout).await
    }
}

struct RecordedEndpoint {
    inner: Box<dyn UdpEndpoint>,
    channel: Channel<SignalingNetworkEvent>,
    recorder: Recorder,
    ctx: RunContext,
    bind_key: LaneKey,
    roles: HashSet<UaRole>,
    rules: Arc<Vec<Arc<dyn PeerAuditRule>>>,
    should_audit: ShouldAuditBind,
    /// Whether the inner endpoint accepted the delivery tap. `true` → arrivals
    /// are recorded at delivery and `recv` records only consumption; `false` →
    /// legacy fallback, `recv` records arrival + consumption adjacently.
    tapped: bool,
}

impl RecordedEndpoint {
    fn record_read(&self, p: &UdpPacket) {
        if !self.tapped {
            self.channel.record(SignalingNetworkEvent::RecvItem {
                bind_key: self.bind_key.clone(),
                packet: p.clone(),
                disposition: RecvDisposition::Delivered,
            });
        }
        self.channel.record(SignalingNetworkEvent::RecvConsumed {
            bind_key: self.bind_key.clone(),
            packet: p.clone(),
        });
    }
}

#[async_trait]
impl UdpEndpoint for RecordedEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        let bind_key = self.bind_key.clone();
        record_call(
            &self.channel,
            SignalingNetworkEvent::SendCalled {
                bind_key: bind_key.clone(),
                to: dst,
                msg: buf.to_vec(),
            },
            |outcome: CallOutcome<'_, (), SendError>| {
                let kind = match outcome {
                    CallOutcome::Ok(_) => SendOutcome::Ok,
                    CallOutcome::Err(_) => SendOutcome::Err,
                };
                Some(SignalingNetworkEvent::SendResult {
                    bind_key: bind_key.clone(),
                    outcome: kind,
                })
            },
            self.inner.send_to(buf, dst),
        )
        .await
    }

    async fn recv(&self) -> Option<UdpPacket> {
        let pkt = self.inner.recv().await;
        if let Some(p) = &pkt {
            self.record_read(p);
        }
        pkt
    }

    fn try_recv(&self) -> Option<UdpPacket> {
        let pkt = self.inner.try_recv();
        if let Some(p) = &pkt {
            self.record_read(p);
        }
        pkt
    }

    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.inner.counters()
    }
}

impl Drop for RecordedEndpoint {
    fn drop(&mut self) {
        // Per-bind scope close (RAII analogue of the source's bind finalizer).
        self.channel.record(SignalingNetworkEvent::BindRelease {
            bind_key: self.bind_key.clone(),
        });

        // Queue-leak: capture depth BEFORE the inner endpoint drops and closes
        // its queue. Advisory — many fixtures release a bind with packets
        // still queued (uncollected keepalive replies, ...).
        let depth = self.inner.queue_depth();
        if depth > 0 {
            let seq = self.recorder.sequencer().next();
            self.recorder.record_anomaly(RecordedAnomaly::new(
                "queueLeak",
                "queueLeak",
                format!("{depth} packets queued at bind close"),
                Severity::Advisory,
                Some(self.bind_key.clone()),
                seq,
                now_ms(),
            ));
        }

        // Per-bind rules.
        if !self.ctx.rules_enabled() || self.rules.is_empty() {
            return;
        }
        if !(self.should_audit)(&self.bind_key) {
            return;
        }
        let slice: Vec<Stamped<SignalingNetworkEvent>> = self
            .channel
            .snapshot()
            .into_iter()
            .filter(|s| s.event.bind_key() == &self.bind_key && audit_visible_event(&s.event))
            .collect();
        for rule in self.rules.iter() {
            if !subject_intersects(&rule.subject(), &self.roles) {
                continue;
            }
            for v in rule.check(&slice, &self.bind_key) {
                let severity = if rule.force_advisory() {
                    Severity::Advisory
                } else {
                    self.ctx.severity_for(SIGNALING_TAG, false)
                };
                let seq = self.recorder.sequencer().next();
                self.recorder.record_anomaly(RecordedAnomaly::new(
                    "signalingAudit",
                    rule.name(),
                    v,
                    severity,
                    Some(self.bind_key.clone()),
                    seq,
                    now_ms(),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ParanoidSignalingNetwork (paranoidInputs equivalent)
// ---------------------------------------------------------------------------

use crate::types::MAX_UDP_PAYLOAD;
use layer_harness::ParanoidViolation;

/// Caller-side precondition checks. A violation is a programmer error and is
/// raised by panic (the Rust analogue of `Effect.die`). Checks (all always-on,
/// µs-scale):
///
///   - `PA2_bindOpts_queueMax`  bind_udp queue_max ≥ 1
///   - `PA3_send_validDest`     send dst port ≠ 0
///   - `PA4_send_msgBuffer`     send buf non-empty
///   - `PA5_send_msgSizeBound`  send buf.len() ≤ MAX_UDP_PAYLOAD
///
/// (PA1 — a valid bind address — is enforced by the `SocketAddr` type itself:
/// an ip and a u16 port always exist, and port 0 = ephemeral is allowed.)
#[derive(Clone)]
pub struct ParanoidSignalingNetwork {
    inner: Arc<dyn SignalingNetwork>,
}

impl ParanoidSignalingNetwork {
    pub fn new(inner: Arc<dyn SignalingNetwork>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl SignalingNetwork for ParanoidSignalingNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        if opts.queue_max == 0 {
            panic!(
                "{}",
                ParanoidViolation::new(
                    "PA2_bindOpts_queueMax",
                    "bind_udp queue_max must be a positive integer (got 0)",
                )
            );
        }
        let endpoint = self.inner.bind_udp(opts).await?;
        Ok(Box::new(ParanoidEndpoint { inner: endpoint }))
    }

    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket> {
        self.inner.drain_undeliverable().await
    }
    fn transit_delay_ms(&self) -> Option<u64> {
        self.inner.transit_delay_ms()
    }
    fn in_flight(&self) -> i64 {
        self.inner.in_flight()
    }
    fn bump_in_flight(&self, delta: i64) {
        self.inner.bump_in_flight(delta)
    }
    fn queue_depths(&self) -> Vec<(SocketAddr, usize)> {
        self.inner.queue_depths()
    }
    async fn await_in_flight(&self, timeout: Duration) {
        self.inner.await_in_flight(timeout).await
    }
}

struct ParanoidEndpoint {
    inner: Box<dyn UdpEndpoint>,
}

#[async_trait]
impl UdpEndpoint for ParanoidEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        if dst.port() == 0 {
            panic!(
                "{}",
                ParanoidViolation::new(
                    "PA3_send_validDest",
                    format!("send dst port must be in 1..=65535 (got {})", dst.port()),
                )
            );
        }
        if buf.is_empty() {
            panic!(
                "{}",
                ParanoidViolation::new("PA4_send_msgBuffer", "send buf must be non-empty")
            );
        }
        if buf.len() > MAX_UDP_PAYLOAD {
            panic!(
                "{}",
                ParanoidViolation::new(
                    "PA5_send_msgSizeBound",
                    format!("send buf.len()={} exceeds MAX_UDP_PAYLOAD={MAX_UDP_PAYLOAD}", buf.len()),
                )
            );
        }
        self.inner.send_to(buf, dst).await
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.inner.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.inner.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.inner.counters()
    }
}

// ---------------------------------------------------------------------------
// Canonical composition forwarder
// ---------------------------------------------------------------------------

/// A wrapped network plus the recording handle needed to drive the layer-close
/// audit. `network` is what consumers use (the outer decorator); `recording`
/// is the inner recording handle for `close()` / recorder assertions.
pub struct WrappedNetwork {
    pub network: Arc<dyn SignalingNetwork>,
    pub recording: RecordingSignalingNetwork,
}

/// Compose the contract decorators in canonical order
/// `paranoidInputs(scopedAudit(impl))`. Set `paranoid = false` to skip the
/// precondition layer (perf benches, deliberate-violation tests).
pub fn with_all_contracts(
    impl_: Arc<dyn SignalingNetwork>,
    recorder: Recorder,
    ctx: RunContext,
    opts: ScopedAuditOptions,
    paranoid: bool,
) -> WrappedNetwork {
    let recording = RecordingSignalingNetwork::new(impl_, recorder, ctx, opts);
    let recording_handle = recording.clone();
    let network: Arc<dyn SignalingNetwork> = if paranoid {
        Arc::new(ParanoidSignalingNetwork::new(Arc::new(recording)))
    } else {
        Arc::new(recording)
    };
    WrappedNetwork {
        network,
        recording: recording_handle,
    }
}
