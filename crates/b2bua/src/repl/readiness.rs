//! [`Readiness`] — the b2bua readiness state machine (ADR-0011 X6 / ADR-0010
//! X8). Drives the self-reported OPTIONS answer a front proxy probes
//! (`crates/sip-proxy/src/health/probe.rs`) on two axes (ADR-0050): serving
//! dialogs, and accepting new calls ([`Readiness::options_report`]):
//!
//! - [`ReadinessState::NotReady`] → `503`: serves nothing yet.
//! - [`ReadinessState::Ready`] → `200`, `new=accept`, or `new=refuse;
//!   cause=backend` while a gating backend's breaker is open.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

use super::backend_gate::BackendGate;
use super::ReplicationSupervisor;

/// The three readiness states the OPTIONS responder self-reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessState {
    /// Not yet re-hydrated / caught up — proxy must not route new calls here.
    NotReady,
    /// Re-hydrated + caught up (latched once reached).
    Ready,
    /// Shutting down (SIGTERM) — terminal; proxy drains traffic away.
    Draining,
}

/// Why a node that serves dialogs refuses new calls: the `cause` of its
/// `X-Readiness` (ADR-0050 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A gating backend's breaker is open.
    Backend,
    /// The node is draining (SIGTERM or withdrawal).
    Draining,
}

impl Refusal {
    /// The `cause` text.
    pub const fn cause(self) -> &'static str {
        match self {
            Refusal::Backend => "backend",
            Refusal::Draining => "draining",
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
    /// Whether the "still NotReady, and why" line has been emitted. The state
    /// machine is monotone (NotReady → Ready → Draining), so each transition —
    /// including the boot state — is lined exactly once (ADR-0026): `state` is
    /// read once per OPTIONS probe and must never be a per-probe line.
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
            tracing::info!(
                node = %self.inner.node,
                state = "Draining",
                reason,
                "readiness transition"
            );
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

    /// What the OPTIONS answer reports, from [`state`](Self::state) and the
    /// backend gate: the one place the two axes are derived.
    pub fn options_report(&self) -> OptionsReport {
        match self.state() {
            ReadinessState::NotReady => OptionsReport::NotReady,
            ReadinessState::Draining => OptionsReport::Serving(Some(Refusal::Draining)),
            ReadinessState::Ready if self.inner.backends.any_open() => {
                OptionsReport::Serving(Some(Refusal::Backend))
            }
            ReadinessState::Ready => OptionsReport::Serving(None),
        }
    }

    /// The current readiness state (Draining wins; else latched/gated Ready;
    /// else NotReady). Latches `Ready` the first time the gates are all true —
    /// and only over a SETTLED membership view, so the informer's empty boot
    /// snapshot never vacuously latches a node before its bound (ADR-0045).
    pub fn state(&self) -> ReadinessState {
        if self.inner.draining.load(Ordering::SeqCst) {
            return ReadinessState::Draining;
        }
        if self.inner.ready_latched.load(Ordering::SeqCst) {
            return ReadinessState::Ready;
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
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A flip-able source for truth-table + transition tests.
    struct FlagSource {
        bootstrapped: AtomicBool,
        current: AtomicBool,
    }
    impl FlagSource {
        fn new(b: bool, c: bool) -> Arc<Self> {
            Arc::new(Self { bootstrapped: AtomicBool::new(b), current: AtomicBool::new(c) })
        }
        fn set(&self, b: bool, c: bool) {
            self.bootstrapped.store(b, Ordering::SeqCst);
            self.current.store(c, Ordering::SeqCst);
        }
    }
    impl ReadinessSource for FlagSource {
        fn all_bootstrapped(&self) -> bool {
            self.bootstrapped.load(Ordering::SeqCst)
        }
        fn all_current(&self) -> bool {
            self.current.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn truth_table_before_latch() {
        // (bootstrapped, current) → state, with a fresh (un-latched) Readiness.
        for (b, c, want) in [
            (false, false, ReadinessState::NotReady),
            (true, false, ReadinessState::NotReady),
            (false, true, ReadinessState::NotReady),
            (true, true, ReadinessState::Ready),
        ] {
            let r = Readiness::new(FlagSource::new(b, c));
            assert_eq!(r.state(), want, "({b},{c})");
        }
    }

    /// A source whose gates are vacuously true over an UNSYNCED membership view
    /// (the informer's empty boot snapshot) must NOT latch Ready: peers — and
    /// the calls to reclaim from them — are not visible yet.
    struct UnsyncedSource {
        synced: AtomicBool,
    }
    impl ReadinessSource for UnsyncedSource {
        fn all_bootstrapped(&self) -> bool {
            true // vacuous .all() over the empty peer set
        }
        fn all_current(&self) -> bool {
            true // vacuous
        }
        fn membership_settled(&self) -> bool {
            self.synced.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn vacuous_gates_cannot_latch_before_membership_syncs() {
        let src = Arc::new(UnsyncedSource { synced: AtomicBool::new(false) });
        let r = Readiness::new(src.clone());
        assert_eq!(
            r.state(),
            ReadinessState::NotReady,
            "empty-but-unsynced membership must not vacuously latch Ready"
        );
        // The informer's initial LIST completes; an (actually) empty cluster —
        // or one whose gates are genuinely satisfied — may now latch.
        src.synced.store(true, Ordering::SeqCst);
        assert_eq!(r.state(), ReadinessState::Ready);
    }

    #[test]
    fn ready_latches_against_blip() {
        let src = FlagSource::new(true, true);
        let r = Readiness::new(src.clone());
        assert_eq!(r.state(), ReadinessState::Ready);
        // A transient peer blip drops current — readiness must NOT revert.
        src.set(true, false);
        assert_eq!(r.state(), ReadinessState::Ready);
        src.set(false, false);
        assert_eq!(r.state(), ReadinessState::Ready);
    }

    #[test]
    fn draining_wins_over_latched_ready() {
        let r = Readiness::new(FlagSource::new(true, true));
        assert_eq!(r.state(), ReadinessState::Ready);
        r.set_draining();
        assert_eq!(r.state(), ReadinessState::Draining);
    }

    #[test]
    fn draining_wins_over_not_ready_and_is_terminal() {
        let src = FlagSource::new(false, false);
        let r = Readiness::new(src.clone());
        assert_eq!(r.state(), ReadinessState::NotReady);
        r.set_draining();
        assert_eq!(r.state(), ReadinessState::Draining);
        // Even if the gate opens afterwards, Draining is terminal.
        src.set(true, true);
        assert_eq!(r.state(), ReadinessState::Draining);
    }

    #[test]
    fn always_ready_is_ready() {
        let r = Readiness::always_ready();
        assert_eq!(r.state(), ReadinessState::Ready);
        r.set_draining();
        assert_eq!(r.state(), ReadinessState::Draining);
    }

    /// A gating backend's open breaker refuses new calls without a latch,
    /// the node still serving; `state` (the `/ready` surface) never follows it.
    #[test]
    fn an_open_backend_breaker_refuses_new_calls_only_and_never_latches() {
        let r = Readiness::always_ready();
        let (breaker, open) = watch::channel(false);
        r.gate_options_on(open);
        assert_eq!(r.options_report(), OptionsReport::Serving(None));
        breaker.send_replace(true);
        assert_eq!(
            r.options_report(),
            OptionsReport::Serving(Some(Refusal::Backend)),
            "open: serving, refusing new calls"
        );
        assert_eq!(r.state(), ReadinessState::Ready, "/ready unchanged");
        breaker.send_replace(false);
        assert_eq!(r.options_report(), OptionsReport::Serving(None), "closed: back, no latch");
        breaker.send_replace(true);
        r.set_draining();
        assert_eq!(
            r.options_report(),
            OptionsReport::Serving(Some(Refusal::Draining)),
            "draining wins"
        );
    }

    /// Before the node is ready, the gate changes nothing.
    #[test]
    fn a_not_ready_node_stays_not_ready_whatever_the_gate() {
        let r = Readiness::new(FlagSource::new(false, false));
        let (breaker, open) = watch::channel(false);
        r.gate_options_on(open);
        assert_eq!(r.options_report(), OptionsReport::NotReady);
        breaker.send_replace(true);
        assert_eq!(r.options_report(), OptionsReport::NotReady);
    }

    /// Each state's report: NotReady serves nothing, Ready accepts, Draining
    /// serves and refuses.
    #[test]
    fn each_state_reports_its_two_axes() {
        let src = FlagSource::new(false, false);
        let r = Readiness::new(src.clone());
        assert_eq!(r.options_report(), OptionsReport::NotReady);
        src.set(true, true);
        assert_eq!(r.options_report(), OptionsReport::Serving(None));
        r.set_draining();
        assert_eq!(r.options_report(), OptionsReport::Serving(Some(Refusal::Draining)));
        assert_eq!(Refusal::Backend.cause(), "backend");
        assert_eq!(Refusal::Draining.cause(), "draining");
    }
}
