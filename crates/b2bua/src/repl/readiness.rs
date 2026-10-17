//! [`Readiness`] — the b2bua readiness state machine (ADR-0011 X6 / ADR-0010
//! X8). Drives the self-reported OPTIONS answer a front proxy probes
//! (`crates/sip-proxy/src/health/probe.rs`) on two axes (ADR-0050): serving
//! dialogs, and accepting new calls ([`Readiness::options_report`]):
//!
//! - [`ReadinessState::NotReady`] → `503`: serves nothing yet.
//! - [`ReadinessState::Ready`] → `200`, `new=accept`, or `new=refuse;
//!   cause=backend` while a gating backend's breaker is open.
//! - [`ReadinessState::Cordoned`] → `200`, `new=refuse; cause=cordoned`.
//! - [`ReadinessState::Draining`] → `200`, `new=refuse; cause=draining`.
//!
//! ## Gating
//! Readiness rides two sticky cluster signals exposed by
//! [`ReplicationSupervisor`](super::ReplicationSupervisor): `all_bootstrapped`
//! (every reachable peer re-hydrated) AND `all_current` (every peer's forward
//! replog caught up). Both true ⇒ the node may serve.
//!
//! ## Latch + Draining precedence
//! Once the gate has *ever* opened, [`Readiness::state`] latches `Ready`
//! (`ready_latched`): a transient peer blip that flips `all_current` back to
//! false must NOT flap a serving node to `NotReady` (X6 — current is itself
//! sticky; readiness shouldn't oscillate). `Draining` is terminal and always
//! wins over a latched `Ready` (SIGTERM → drain, never un-drain). The latch has
//! two sources: SIGTERM, and the worker observing its own endpoint withdrawn
//! from routing (ADR-0031 D6) — whichever comes first.
//!
//! ## Backend gate (new calls only)
//! [`Readiness::gate_options_on`] registers a backend's breaker: while one is
//! open, [`Readiness::options_report`] reports a `Ready` node refusing new
//! calls, and accepting again once all are closed; never latched (ADR-0043).
//! [`Readiness::state`], which `/ready` reads, ignores the gate.
//!
//! ## Cordon (operator, ADR-0050 §9)
//! An operator cordons a `Ready` node and uncordons it: `Ready ⇄ Cordoned`,
//! with no timer. A cordoned node refuses every new call and serves every
//! dialog; `/ready` reads it as `Ready`. A cordon is refused while the node is
//! NotReady or Draining; SIGTERM or withdrawal drains a cordoned node as a
//! `Ready` one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

use super::backend_gate::BackendGate;
use super::ReplicationSupervisor;

/// The readiness states the OPTIONS responder self-reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessState {
    /// Not yet re-hydrated / caught up — proxy must not route new calls here.
    NotReady,
    /// Re-hydrated + caught up (latched once reached).
    Ready,
    /// Ready, and cordoned by an operator: refuses new calls, serves every
    /// dialog; reversible.
    Cordoned,
    /// Shutting down (SIGTERM) — terminal; proxy drains traffic away.
    Draining,
}

impl ReadinessState {
    /// The state's name as an operator route reports it.
    pub const fn label(self) -> &'static str {
        match self {
            ReadinessState::NotReady => "not-ready",
            ReadinessState::Ready => "ready",
            ReadinessState::Cordoned => "cordoned",
            ReadinessState::Draining => "draining",
        }
    }
}

/// Why a node that serves dialogs refuses new calls: the `cause` of its
/// `X-Readiness` (ADR-0050 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A gating backend's breaker is open.
    Backend,
    /// An operator cordoned the node.
    Cordoned,
    /// The node is draining (SIGTERM or withdrawal).
    Draining,
}

impl Refusal {
    /// The `cause` text.
    pub const fn cause(self) -> &'static str {
        match self {
            Refusal::Backend => "backend",
            Refusal::Cordoned => "cordoned",
            Refusal::Draining => "draining",
        }
    }
}

/// Why a cordon request was refused: the node is in a state no operator
/// cordon applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CordonRefused {
    /// Not yet bootstrapped: it takes no call to refuse.
    NotReady,
    /// Draining: terminal, it never accepts new calls again.
    Draining,
}

impl CordonRefused {
    /// The state the request was refused in, as [`ReadinessState::label`]
    /// names it.
    pub const fn label(self) -> &'static str {
        match self {
            CordonRefused::NotReady => ReadinessState::NotReady.label(),
            CordonRefused::Draining => ReadinessState::Draining.label(),
        }
    }
}

/// What a node's answer to its peer proxy's OPTIONS reports (ADR-0050 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsReport {
    /// Not yet bootstrapped: serves nothing.
    NotReady,
    /// Serves its dialogs; refuses new calls for the reason given, or accepts
    /// them (`None`).
    Serving(Option<Refusal>),
}

/// The two cluster gates readiness reads. Implemented for
/// [`ReplicationSupervisor`]; a trivial always-true impl backs
/// [`Readiness::always_ready`] for an unwired node.
pub trait ReadinessSource: Send + Sync {
    /// Every reachable peer has finished Bootstrap re-hydration.
    fn all_bootstrapped(&self) -> bool;
    /// Every peer's forward replog is caught up — sticky `current`.
    fn all_current(&self) -> bool;
    /// The membership view behind the two gates may be read: it is
    /// authoritative (the informer's initial LIST completed), or it has had its
    /// bound to become so. Both gates are `.all()` over the desired peer set —
    /// vacuously TRUE over the empty snapshot an informer starts with — so
    /// without this gate a rebooting node could latch Ready in the first
    /// instants of boot, before its peers (and the calls to reclaim from them)
    /// were even visible. Default `true` for sources with no async view
    /// (always-ready / static / tests).
    fn membership_settled(&self) -> bool {
        true
    }
    /// This node's own endpoint left the slice (ADR-0031 D6): its orchestrator
    /// removed it, and its backups took its calls with an INVITE in flight
    /// over (ADR-0047). Default `false` for a source that does not observe the
    /// node itself.
    fn left_the_slice(&self) -> bool {
        false
    }
}

impl ReadinessSource for ReplicationSupervisor {
    fn all_bootstrapped(&self) -> bool {
        ReplicationSupervisor::all_bootstrapped(self)
    }
    fn all_current(&self) -> bool {
        ReplicationSupervisor::all_current(self)
    }
    fn membership_settled(&self) -> bool {
        ReplicationSupervisor::membership_settled(self)
    }
    fn left_the_slice(&self) -> bool {
        self.self_endpoint().borrow().left_the_slice()
    }
}

/// A trivial source that is always bootstrapped + current. Backs the
/// unwired node (no replication wired) so OPTIONS keeps answering 200.
struct AlwaysReadySource;

impl ReadinessSource for AlwaysReadySource {
    fn all_bootstrapped(&self) -> bool {
        true
    }
    fn all_current(&self) -> bool {
        true
    }
}

struct ReadinessInner {
    source: Arc<dyn ReadinessSource>,
    /// Drain latch (SIGTERM or self-observed withdrawal) — once set,
    /// [`Readiness::state`] is terminally `Draining`.
    draining: AtomicBool,
    /// Sticky readiness — once the gate opens it stays open (X6 anti-flap).
    ready_latched: AtomicBool,
    /// The operator's cordon: set only over a latched `Ready`, so a set flag
    /// reads `Cordoned` until it is cleared or the node drains.
    cordoned: AtomicBool,
    /// Whether the "still NotReady, and why" line has been emitted. The
    /// latched states are monotone (NotReady → Ready → Draining), so each
    /// transition — including the boot state — is lined exactly once
    /// (ADR-0026): `state` is read once per OPTIONS probe and must never be a
    /// per-probe line. A cordon change is lined by the operator call making it.
    not_ready_logged: AtomicBool,
    /// The backends whose open breaker gates the OPTIONS self-health.
    backends: BackendGate,
    /// The node this readiness belongs to: a transition is lined as that node
    /// whoever reads the state (a probe, or an observer serving no node).
    node: observe::NodeScope,
}

/// Clone-cheap readiness handle (shared `Arc` inside). Wired into
/// [`RouterCtx`](crate::router::RouterCtx) and read by the OPTIONS responder.
#[derive(Clone)]
pub struct Readiness {
    inner: Arc<ReadinessInner>,
    /// The process's boot generation the OPTIONS answer states
    /// (`X-Incarnation`, ADR-0050 §2).
    incarnation: u64,
}

impl Readiness {
    /// Build readiness over a cluster [`ReadinessSource`] (the supervisor),
    /// belonging to the node the caller serves, at incarnation 0.
    pub fn new(source: Arc<dyn ReadinessSource>) -> Self {
        Self {
            inner: Arc::new(ReadinessInner {
                source,
                draining: AtomicBool::new(false),
                ready_latched: AtomicBool::new(false),
                cordoned: AtomicBool::new(false),
                not_ready_logged: AtomicBool::new(false),
                backends: BackendGate::default(),
                node: observe::NodeScope::current(),
            }),
            incarnation: 0,
        }
    }

    /// An unwired node's readiness: always `Ready` (until drained).
    pub fn always_ready() -> Self {
        Self::new(Arc::new(AlwaysReadySource))
    }

    /// This readiness, stating `incarnation` as the process's boot generation.
    pub fn with_incarnation(mut self, incarnation: u64) -> Self {
        self.incarnation = incarnation;
        self
    }

    /// The process's boot generation.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Mark the node Draining (SIGTERM). Terminal — never reverts.
    pub fn set_draining(&self) {
        self.latch_draining("shutdown requested");
    }

    /// Mark the node Draining because it observed its own endpoint withdrawn
    /// from routing (ADR-0031 D6). Terminal; a no-op after SIGTERM already did.
    pub fn set_withdrawn(&self) {
        self.latch_draining("withdrawn from routing");
    }

    fn latch_draining(&self, reason: &'static str) {
        if !self.inner.draining.swap(true, Ordering::SeqCst) {
            self.line_transition("Draining", reason);
        }
    }

    /// Whether this node's own endpoint left the slice
    /// ([`ReadinessSource::left_the_slice`]).
    pub fn left_the_slice(&self) -> bool {
        self.inner.source.left_the_slice()
    }

    /// Whether the drain latch is set — the raw SIGTERM flag, read WITHOUT
    /// evaluating (or latching) the readiness gates. Introspection only;
    /// [`state`](Self::state) remains the state machine.
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::SeqCst)
    }

    /// From now on, `breaker` open (`true`) makes a `Ready` node refuse new
    /// calls ([`options_report`](Self::options_report)).
    pub fn gate_options_on(&self, breaker: watch::Receiver<bool>) {
        self.inner.backends.add(breaker);
    }

    /// Cordon the node: `Ready → Cordoned`; idempotent on a cordoned node.
    /// Refused while NotReady or Draining.
    pub fn cordon(&self) -> Result<(), CordonRefused> {
        match self.state() {
            ReadinessState::NotReady => Err(CordonRefused::NotReady),
            ReadinessState::Draining => Err(CordonRefused::Draining),
            ReadinessState::Ready | ReadinessState::Cordoned => {
                if !self.inner.cordoned.swap(true, Ordering::SeqCst) {
                    self.line_transition("Cordoned", "operator cordon");
                }
                Ok(())
            }
        }
    }

    /// Lift the cordon: `Cordoned → Ready`; idempotent on a node no cordon
    /// holds. Refused while Draining.
    pub fn uncordon(&self) -> Result<(), CordonRefused> {
        if self.state() == ReadinessState::Draining {
            return Err(CordonRefused::Draining);
        }
        if self.inner.cordoned.swap(false, Ordering::SeqCst) {
            self.line_transition("Ready", "operator uncordon");
        }
        Ok(())
    }

    /// Whether an operator cordon is set, read apart from the state: a node
    /// that drains keeps the cordon it was given.
    pub fn is_cordoned(&self) -> bool {
        self.inner.cordoned.load(Ordering::SeqCst)
    }

    fn line_transition(&self, state: &'static str, reason: &'static str) {
        tracing::info!(node = %self.inner.node, state, reason, "readiness transition");
    }

    /// What the OPTIONS answer reports, from [`state`](Self::state) and the
    /// backend gate: the one place the two axes are derived. A cordoned node
    /// names the cordon as its cause whatever the backend gate reads.
    pub fn options_report(&self) -> OptionsReport {
        match self.state() {
            ReadinessState::NotReady => OptionsReport::NotReady,
            ReadinessState::Draining => OptionsReport::Serving(Some(Refusal::Draining)),
            ReadinessState::Cordoned => OptionsReport::Serving(Some(Refusal::Cordoned)),
            ReadinessState::Ready if self.inner.backends.any_open() => {
                OptionsReport::Serving(Some(Refusal::Backend))
            }
            ReadinessState::Ready => OptionsReport::Serving(None),
        }
    }

    /// The current readiness state (Draining wins; else latched/gated Ready,
    /// Cordoned while an operator cordon holds; else NotReady). Latches
    /// `Ready` the first time the gates are all true — and only over a SETTLED
    /// membership view, so the informer's empty boot snapshot never vacuously
    /// latches a node before its bound (ADR-0045).
    pub fn state(&self) -> ReadinessState {
        if self.inner.draining.load(Ordering::SeqCst) {
            return ReadinessState::Draining;
        }
        if self.inner.ready_latched.load(Ordering::SeqCst) {
            return match self.inner.cordoned.load(Ordering::SeqCst) {
                true => ReadinessState::Cordoned,
                false => ReadinessState::Ready,
            };
        }
        let settled = self.inner.source.membership_settled();
        let bootstrapped = self.inner.source.all_bootstrapped();
        let current = self.inner.source.all_current();
        if settled && bootstrapped && current {
            // `swap`, not `store`: two probes racing the opening gate must
            // produce ONE transition line.
            if !self.inner.ready_latched.swap(true, Ordering::SeqCst) {
                tracing::info!(
                    node = %self.inner.node,
                    state = "Ready",
                    reason = "membership settled, every peer bootstrapped and current",
                    "readiness transition"
                );
            }
            return ReadinessState::Ready;
        }
        if !self.inner.not_ready_logged.swap(true, Ordering::SeqCst) {
            tracing::info!(
                node = %self.inner.node,
                state = "NotReady",
                membership_settled = settled,
                all_bootstrapped = bootstrapped,
                all_current = current,
                "readiness transition"
            );
        }
        ReadinessState::NotReady
    }
}

#[cfg(test)]
mod tests;
