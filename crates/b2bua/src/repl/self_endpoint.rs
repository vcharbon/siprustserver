//! [`SelfEndpoint`] — a worker's view of **its own** endpoint in the membership
//! it pulls its peers from (ADR-0031 D6).
//!
//! The informer shows a worker every endpoint of its pool, its own included; the
//! proxy routes to the same slice. So the one predicate the proxy applies to
//! this worker's endpoint — is it routable — is a fact the worker can read for
//! itself, instead of inferring it from SIGTERM alone. The observation is a
//! monotone latch: `Unobserved` until the worker has seen itself routable once
//! (a booting pod is absent or not ready before it is published, which is not a
//! withdrawal), then `Routable`, then — sticky — `Withdrawn` with the condition
//! that produced it. It is a fact about routing only: nothing here stops the
//! worker from serving what still reaches it (D5).

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;
use topology::Peer;

/// What the membership showed about this worker's own endpoint the instant it
/// stopped being routable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalCondition {
    /// The endpoint left the membership snapshot entirely.
    Absent,
    /// The endpoint is still in the snapshot, `ready = false` and `terminating`:
    /// the orchestrator is removing the member, which stays a replication peer
    /// until its pod is gone (ADR-0031 D1).
    Terminating,
}

impl WithdrawalCondition {
    /// The condition as a log field value.
    pub fn as_str(self) -> &'static str {
        match self {
            WithdrawalCondition::Absent => "absent",
            WithdrawalCondition::Terminating => "terminating",
        }
    }
}

/// The worker's own endpoint as last observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfEndpoint {
    /// Never seen routable: the source does not show the worker itself, or the
    /// worker is not published yet.
    Unobserved,
    /// Seen routable; the proxy can route new traffic here.
    Routable,
    /// Was routable, and is no longer (sticky).
    Withdrawn(WithdrawalCondition),
}

impl SelfEndpoint {
    pub fn is_withdrawn(self) -> bool {
        matches!(self, SelfEndpoint::Withdrawn(_))
    }
}

/// The latch over a stream of membership snapshots. Inert until
/// [`enable`](Self::enable): a static membership never observes itself.
pub struct SelfObserver {
    self_ordinal: String,
    enabled: AtomicBool,
    state: watch::Sender<SelfEndpoint>,
}

impl SelfObserver {
    pub fn new(self_ordinal: impl Into<String>) -> Self {
        Self {
            self_ordinal: self_ordinal.into(),
            enabled: AtomicBool::new(false),
            state: watch::channel(SelfEndpoint::Unobserved).0,
        }
    }

    /// Start evaluating snapshots. Called once the membership source is known to
    /// show the worker its own endpoint ([`topology::Membership::observes_self`]).
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Evaluate one membership `snapshot`. `synced` is the source's
    /// [`topology::Membership::synced`]: an informer's pre-LIST placeholder says
    /// nothing about this worker. Returns the new state on a transition, `None`
    /// when nothing moved. Logs each transition once, with the condition.
    pub fn observe(&self, snapshot: &[Peer], synced: bool) -> Option<SelfEndpoint> {
        if !self.enabled.load(Ordering::SeqCst) || !synced {
            return None;
        }
        let observed = classify(snapshot, &self.self_ordinal);
        let mut transition = None;
        self.state.send_if_modified(|state| {
            let next = match (*state, observed) {
                (SelfEndpoint::Unobserved, None) => SelfEndpoint::Routable,
                (SelfEndpoint::Routable, Some(condition)) => SelfEndpoint::Withdrawn(condition),
                _ => return false,
            };
            *state = next;
            transition = Some(next);
            true
        });
        match transition {
            Some(SelfEndpoint::Routable) => tracing::info!(
                node = observe::node(),
                ordinal = %self.self_ordinal,
                "own endpoint observed routable"
            ),
            Some(SelfEndpoint::Withdrawn(condition)) => tracing::warn!(
                node = observe::node(),
                ordinal = %self.self_ordinal,
                condition = condition.as_str(),
                "own endpoint withdrawn from routing; latching Draining, still serving"
            ),
            _ => {}
        }
        transition
    }

    pub fn current(&self) -> SelfEndpoint {
        *self.state.borrow()
    }

    pub fn is_withdrawn(&self) -> bool {
        self.current().is_withdrawn()
    }

    /// A receiver that wakes on every transition; its value is the current state.
    pub fn subscribe(&self) -> watch::Receiver<SelfEndpoint> {
        self.state.subscribe()
    }
}

/// `None` when `self_ordinal` still belongs here in `snapshot`, else the
/// condition that withdraws it: gone from the slice, or in it as a `terminating`
/// endpoint the orchestrator is removing. A not-ready endpoint that is NOT
/// terminating is a readiness flap on a member the orchestrator keeps
/// (ADR-0031 case 4): `Draining` is terminal, so latching it there would remove
/// for good a worker that is meant to come back.
fn classify(snapshot: &[Peer], self_ordinal: &str) -> Option<WithdrawalCondition> {
    match snapshot.iter().find(|p| p.ordinal == self_ordinal) {
        Some(p) if p.terminating && !p.ready => Some(WithdrawalCondition::Terminating),
        Some(_) => None,
        None => Some(WithdrawalCondition::Absent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(ords: &[&str]) -> Vec<Peer> {
        ords.iter().map(|o| Peer::new(*o, *o)).collect()
    }

    #[test]
    fn disabled_observer_never_moves() {
        let o = SelfObserver::new("w0");
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None);
        assert_eq!(o.observe(&peers(&["w1"]), true), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
        assert!(!o.is_withdrawn());
    }

    #[test]
    fn absent_before_first_publication_is_not_a_withdrawal() {
        let o = SelfObserver::new("w0");
        o.enable();
        assert_eq!(o.observe(&peers(&[]), true), None);
        assert_eq!(o.observe(&peers(&["w1"]), true), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
    }

    #[test]
    fn a_terminating_own_endpoint_still_in_the_slice_is_a_withdrawal() {
        // The graceful shape: the endpoint stays in the slice (its peers keep
        // pulling it, D1) reading `ready=false, terminating=true`.
        let o = SelfObserver::new("w0");
        o.enable();
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), Some(SelfEndpoint::Routable));
        let terminating =
            vec![Peer::with_conditions("w0", "w0", false, true), Peer::new("w1", "w1")];
        assert_eq!(
            o.observe(&terminating, true),
            Some(SelfEndpoint::Withdrawn(WithdrawalCondition::Terminating))
        );
        assert!(o.is_withdrawn());
    }

    #[test]
    fn a_readiness_flap_is_not_a_withdrawal() {
        // Case 4: not ready, not terminating, same address — the orchestrator
        // keeps the member and expects it back, and `Draining` is terminal.
        let o = SelfObserver::new("w0");
        o.enable();
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), Some(SelfEndpoint::Routable));
        let flapping = vec![Peer::with_conditions("w0", "w0", false, false), Peer::new("w1", "w1")];
        assert_eq!(o.observe(&flapping, true), None);
        assert!(!o.is_withdrawn());
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None, "and it comes back silently");
    }

    #[test]
    fn an_unsynced_snapshot_is_ignored() {
        let o = SelfObserver::new("w0");
        o.enable();
        assert_eq!(o.observe(&peers(&["w0"]), false), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
    }

    #[test]
    fn routable_then_absent_latches_withdrawn_once() {
        let o = SelfObserver::new("w0");
        o.enable();
        let mut rx = o.subscribe();
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), Some(SelfEndpoint::Routable));
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None, "steady state is silent");
        assert_eq!(*rx.borrow_and_update(), SelfEndpoint::Routable);

        assert_eq!(
            o.observe(&peers(&["w1"]), true),
            Some(SelfEndpoint::Withdrawn(WithdrawalCondition::Absent))
        );
        assert!(rx.has_changed().unwrap());
        assert!(rx.borrow_and_update().is_withdrawn());
        assert!(o.is_withdrawn());

        // Sticky: a re-publication does not un-withdraw (Draining is terminal).
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None);
        assert_eq!(o.observe(&peers(&["w1"]), true), None);
        assert!(o.is_withdrawn());
        assert!(!rx.has_changed().unwrap());
    }
}
