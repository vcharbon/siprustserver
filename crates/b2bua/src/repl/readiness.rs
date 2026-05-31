//! [`Readiness`] — the b2bua readiness state machine (migration slice S7,
//! ADR-0011 X6 / ADR-0010 X8). Drives the self-reported OPTIONS health a front
//! proxy probes (`crates/sip-proxy/src/health/probe.rs`):
//!
//! - [`ReadinessState::Ready`] → `200 OK`.
//! - [`ReadinessState::NotReady`] → `503` + `Reason: SIP;cause=503;text="not-ready"`.
//! - [`ReadinessState::Draining`] → `503` + `Reason: SIP;cause=503;text="draining"`
//!   + `Retry-After: 0`.
//!
//! ## Gating
//! Readiness rides two sticky cluster signals exposed by
//! [`ReplicationSupervisor`](super::ReplicationSupervisor): `all_bootstrapped`
//! (every reachable peer re-hydrated, S6) AND `all_current` (every peer's
//! forward replog caught up, S5). Both true ⇒ the node may serve.
//!
//! ## Latch + Draining precedence
//! Once the gate has *ever* opened, [`Readiness::state`] latches `Ready`
//! (`ready_latched`): a transient peer blip that flips `all_current` back to
//! false must NOT flap a serving node to `NotReady` (X6 — current is itself
//! sticky; readiness shouldn't oscillate). `Draining` is terminal and always
//! wins over a latched `Ready` (SIGTERM → drain, never un-drain).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// The two cluster gates readiness reads. Implemented for
/// [`ReplicationSupervisor`]; a trivial always-true impl backs
/// [`Readiness::always_ready`] for the legacy/default path.
pub trait ReadinessSource: Send + Sync {
    /// Every reachable peer has finished Bootstrap re-hydration (S6).
    fn all_bootstrapped(&self) -> bool;
    /// Every peer's forward replog is caught up — sticky `current` (S5).
    fn all_current(&self) -> bool;
}

impl ReadinessSource for ReplicationSupervisor {
    fn all_bootstrapped(&self) -> bool {
        ReplicationSupervisor::all_bootstrapped(self)
    }
    fn all_current(&self) -> bool {
        ReplicationSupervisor::all_current(self)
    }
}

/// A trivial source that is always bootstrapped + current. Backs the
/// default/legacy path (no replication wired) so OPTIONS keeps answering 200.
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
    /// SIGTERM latch — once set, [`Readiness::state`] is terminally `Draining`.
    draining: AtomicBool,
    /// Sticky readiness — once the gate opens it stays open (X6 anti-flap).
    ready_latched: AtomicBool,
}

/// Clone-cheap readiness handle (shared `Arc` inside). Wired into
/// [`RouterCtx`](crate::router::RouterCtx) and read by the OPTIONS responder.
#[derive(Clone)]
pub struct Readiness {
    inner: Arc<ReadinessInner>,
}

impl Readiness {
    /// Build readiness over a cluster [`ReadinessSource`] (the supervisor).
    pub fn new(source: Arc<dyn ReadinessSource>) -> Self {
        Self {
            inner: Arc::new(ReadinessInner {
                source,
                draining: AtomicBool::new(false),
                ready_latched: AtomicBool::new(false),
            }),
        }
    }

    /// Default/legacy readiness: always `Ready` (until drained). Keeps the
    /// always-200 OPTIONS contract for nodes with no replication wired.
    pub fn always_ready() -> Self {
        Self::new(Arc::new(AlwaysReadySource))
    }

    /// Mark the node Draining (SIGTERM). Terminal — never reverts.
    pub fn set_draining(&self) {
        self.inner.draining.store(true, Ordering::SeqCst);
    }

    /// The current readiness state (Draining wins; else latched/gated Ready;
    /// else NotReady). Latches `Ready` the first time both gates are true.
    pub fn state(&self) -> ReadinessState {
        if self.inner.draining.load(Ordering::SeqCst) {
            return ReadinessState::Draining;
        }
        if self.inner.ready_latched.load(Ordering::SeqCst) {
            return ReadinessState::Ready;
        }
        if self.inner.source.all_bootstrapped() && self.inner.source.all_current() {
            self.inner.ready_latched.store(true, Ordering::SeqCst);
            return ReadinessState::Ready;
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
            Arc::new(Self {
                bootstrapped: AtomicBool::new(b),
                current: AtomicBool::new(c),
            })
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
}
