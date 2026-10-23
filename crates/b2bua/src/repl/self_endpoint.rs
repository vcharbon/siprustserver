//! [`SelfEndpoint`] — a worker's view of **its own** endpoint in the membership
//! it pulls its peers from (ADR-0031 D6, ADR-0055 §6).
//!
//! The informer shows a worker every endpoint of its pool, its own included; the
//! proxy routes to the same slice. So the one predicate the proxy applies to
//! this worker's endpoint — is it routable — is a fact the worker can read for
//! itself, instead of inferring it from SIGTERM alone. The observation is a
//! monotone latch: `Unobserved` until the worker has seen itself in the slice
//! and `ready` once (a booting pod is absent, or present and not ready, before
//! its readiness probe passes, which is not a withdrawal), then `Routable`,
//! then — sticky — `Withdrawn` with the condition that produced it; a `Terminating` withdrawal moves on to `Absent` when the
//! endpoint then leaves the slice. An endpoint that left the slice belongs to a
//! process its orchestrator removed: the ordinal, should it return, names a
//! replacement. The worker latches Draining on it and still serves what
//! reaches it (D5), but applies no routing decision that returns after its
//! endpoint left the slice: its backups took those calls over (ADR-0047 D6).
//!
//! A worker that has seen its ordinal bound to its own worker identity and
//! then sees it bound to another is `Superseded`, from any state and for good:
//! the orchestrator recreated the ordinal while this process still runs. The
//! identities are compared for equality only. Before the worker has seen its
//! own identity, another one is its predecessor's, still listed: no
//! supersession.

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
    /// worker is not published or not ready yet.
    Unobserved,
    /// Seen in the slice and `ready`; the proxy can route new traffic here.
    Routable,
    /// Was routable, and is no longer (sticky).
    Withdrawn(WithdrawalCondition),
    /// The ordinal is bound to another worker identity after this worker saw
    /// it bound to its own (sticky, terminal): this worker authors nothing
    /// more (ADR-0055 §6).
    Superseded,
}

impl SelfEndpoint {
    /// Whether nothing new is routed to this worker: its endpoint was
    /// withdrawn, or its ordinal routes to another worker identity.
    pub fn is_withdrawn(self) -> bool {
        matches!(self, SelfEndpoint::Withdrawn(_) | SelfEndpoint::Superseded)
    }

    /// Whether the endpoint left the slice after it was routable (sticky).
    pub fn left_the_slice(self) -> bool {
        self == SelfEndpoint::Withdrawn(WithdrawalCondition::Absent)
    }

    pub fn is_superseded(self) -> bool {
        self == SelfEndpoint::Superseded
    }
}

/// The latch over a stream of membership snapshots. Inert until
/// [`enable`](Self::enable): a static membership never observes itself.
pub struct SelfObserver {
    self_ordinal: String,
    /// This worker's identity; `None` when it states none, and the ordinal
    /// alone then names its endpoint.
    self_identity: Option<String>,
    /// Set once a snapshot bound the ordinal to `self_identity`.
    own_identity_seen: AtomicBool,
    enabled: AtomicBool,
    state: watch::Sender<SelfEndpoint>,
}

impl SelfObserver {
    pub fn new(self_ordinal: impl Into<String>, self_identity: Option<String>) -> Self {
        Self {
            self_ordinal: self_ordinal.into(),
            self_identity,
            own_identity_seen: AtomicBool::new(false),
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
        let (observed, names_own) =
            classify(snapshot, &self.self_ordinal, self.self_identity.as_deref());
        if names_own {
            self.own_identity_seen.store(true, Ordering::SeqCst);
        }
        let own_identity_seen = self.own_identity_seen.load(Ordering::SeqCst);
        let mut transition = None;
        self.state.send_if_modified(|state| {
            let next = match (*state, observed) {
                (SelfEndpoint::Superseded, _) => return false,
                (_, Sighting::Elsewhere) if own_identity_seen => SelfEndpoint::Superseded,
                (SelfEndpoint::Unobserved, Sighting::Ready) => SelfEndpoint::Routable,
                (SelfEndpoint::Routable, Sighting::Withdrawn(condition)) => {
                    SelfEndpoint::Withdrawn(condition)
                }
                (SelfEndpoint::Routable, Sighting::Elsewhere) => {
                    SelfEndpoint::Withdrawn(WithdrawalCondition::Absent)
                }
                (
                    SelfEndpoint::Withdrawn(WithdrawalCondition::Terminating),
                    Sighting::Withdrawn(WithdrawalCondition::Absent) | Sighting::Elsewhere,
                ) => SelfEndpoint::Withdrawn(WithdrawalCondition::Absent),
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
            Some(SelfEndpoint::Superseded) => tracing::warn!(
                node = observe::node(),
                ordinal = %self.self_ordinal,
                "own ordinal bound to another worker identity; superseded, authoring stops"
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

/// What one snapshot shows of the worker's own endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sighting {
    /// In the slice and `ready`: routable.
    Ready,
    /// In the slice, not ready and not terminating: a pod not ready yet, or a
    /// readiness flap on a member the orchestrator keeps (ADR-0031 case 4).
    NotReady,
    /// Gone from the slice, or in it as a `terminating` endpoint the
    /// orchestrator is removing.
    Withdrawn(WithdrawalCondition),
    /// The ordinal is in the slice bound to another worker identity.
    Elsewhere,
}

/// The [`Sighting`] of `self_ordinal` in `snapshot`, and whether the entry
/// names `self_identity`. An entry naming no identity, or a worker stating
/// none, is read by the ordinal alone. A not-ready endpoint that is NOT
/// terminating is no withdrawal: `Draining` is terminal, so latching it on a
/// readiness flap would remove for good a worker meant to come back.
fn classify(
    snapshot: &[Peer],
    self_ordinal: &str,
    self_identity: Option<&str>,
) -> (Sighting, bool) {
    let Some(p) = snapshot.iter().find(|p| p.ordinal == self_ordinal) else {
        return (Sighting::Withdrawn(WithdrawalCondition::Absent), false);
    };
    let names_own = match (p.identity.as_deref(), self_identity) {
        (Some(theirs), Some(mine)) if theirs != mine => return (Sighting::Elsewhere, false),
        (Some(_), Some(_)) => true,
        _ => false,
    };
    let sighting = match p {
        p if p.ready => Sighting::Ready,
        p if p.terminating => Sighting::Withdrawn(WithdrawalCondition::Terminating),
        _ => Sighting::NotReady,
    };
    (sighting, names_own)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(ords: &[&str]) -> Vec<Peer> {
        ords.iter().map(|o| Peer::new(*o, *o)).collect()
    }

    #[test]
    fn disabled_observer_never_moves() {
        let o = SelfObserver::new("w0", None);
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None);
        assert_eq!(o.observe(&peers(&["w1"]), true), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
        assert!(!o.is_withdrawn());
    }

    #[test]
    fn absent_before_first_publication_is_not_a_withdrawal() {
        let o = SelfObserver::new("w0", None);
        o.enable();
        assert_eq!(o.observe(&peers(&[]), true), None);
        assert_eq!(o.observe(&peers(&["w1"]), true), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
    }

    #[test]
    fn a_published_endpoint_not_ready_yet_is_not_routable() {
        // A booting pod: in the slice, `ready=false, terminating=false`.
        let o = SelfObserver::new("w0", None);
        o.enable();
        let booting = vec![Peer::with_conditions("w0", "w0", false, false), Peer::new("w1", "w1")];
        assert_eq!(o.observe(&booting, true), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), Some(SelfEndpoint::Routable));
    }

    #[test]
    fn a_terminating_own_endpoint_still_in_the_slice_is_a_withdrawal() {
        // The graceful shape: the endpoint stays in the slice (its peers keep
        // pulling it, D1) reading `ready=false, terminating=true`.
        let o = SelfObserver::new("w0", None);
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
    fn a_terminating_endpoint_that_then_leaves_the_slice_has_left_it() {
        // The abrupt shape may show one terminating event before the removal.
        let o = SelfObserver::new("w0", None);
        o.enable();
        o.observe(&peers(&["w0", "w1"]), true);
        let terminating =
            vec![Peer::with_conditions("w0", "w0", false, true), Peer::new("w1", "w1")];
        o.observe(&terminating, true);
        assert!(!o.current().left_the_slice(), "still in the slice");
        assert_eq!(
            o.observe(&peers(&["w1"]), true),
            Some(SelfEndpoint::Withdrawn(WithdrawalCondition::Absent))
        );
        assert!(o.current().left_the_slice());
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None, "a replacement changes nothing");
        assert!(o.current().left_the_slice(), "sticky");
    }

    #[test]
    fn a_readiness_flap_is_not_a_withdrawal() {
        // Case 4: not ready, not terminating, same address — the orchestrator
        // keeps the member and expects it back, and `Draining` is terminal.
        let o = SelfObserver::new("w0", None);
        o.enable();
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), Some(SelfEndpoint::Routable));
        let flapping = vec![Peer::with_conditions("w0", "w0", false, false), Peer::new("w1", "w1")];
        assert_eq!(o.observe(&flapping, true), None);
        assert!(!o.is_withdrawn());
        assert_eq!(o.observe(&peers(&["w0", "w1"]), true), None, "and it comes back silently");
    }

    #[test]
    fn an_unsynced_snapshot_is_ignored() {
        let o = SelfObserver::new("w0", None);
        o.enable();
        assert_eq!(o.observe(&peers(&["w0"]), false), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
    }

    #[test]
    fn routable_then_absent_latches_withdrawn_once() {
        let o = SelfObserver::new("w0", None);
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

    fn bound(ordinal: &str, identity: &str) -> Peer {
        Peer::new(ordinal, ordinal).with_identity(identity)
    }

    fn named(identity: &str) -> SelfObserver {
        let o = SelfObserver::new("w0", Some(identity.to_string()));
        o.enable();
        o
    }

    #[test]
    fn the_ordinal_bound_to_another_identity_supersedes_a_routable_worker() {
        let o = named("uid-a");
        assert_eq!(o.observe(&[bound("w0", "uid-a")], true), Some(SelfEndpoint::Routable));
        assert_eq!(o.observe(&[bound("w0", "uid-b")], true), Some(SelfEndpoint::Superseded));
        assert!(o.current().is_superseded());
        assert!(o.is_withdrawn(), "nothing new is routed to it");
        assert_eq!(o.observe(&[bound("w0", "uid-a")], true), None, "sticky");
        assert_eq!(o.observe(&[], true), None, "terminal");
    }

    #[test]
    fn a_withdrawn_worker_whose_ordinal_returns_under_another_identity_is_superseded() {
        let o = named("uid-a");
        o.observe(&[bound("w0", "uid-a")], true);
        assert_eq!(
            o.observe(&[], true),
            Some(SelfEndpoint::Withdrawn(WithdrawalCondition::Absent))
        );
        let successor = bound("w0", "uid-b").not_ready();
        assert_eq!(o.observe(&[successor], true), Some(SelfEndpoint::Superseded));
    }

    #[test]
    fn a_worker_seen_published_but_never_ready_is_superseded_too() {
        let o = named("uid-a");
        assert_eq!(o.observe(&[bound("w0", "uid-a").not_ready()], true), None);
        assert_eq!(o.observe(&[bound("w0", "uid-b")], true), Some(SelfEndpoint::Superseded));
    }

    #[test]
    fn a_predecessor_still_listed_at_boot_supersedes_nothing() {
        // The recreated worker boots while the slice still binds its ordinal to
        // the process it replaces.
        let o = named("uid-b");
        assert_eq!(o.observe(&[bound("w0", "uid-a")], true), None);
        assert_eq!(o.current(), SelfEndpoint::Unobserved);
        assert_eq!(o.observe(&[bound("w0", "uid-b")], true), Some(SelfEndpoint::Routable));
    }

    #[test]
    fn an_entry_naming_no_identity_never_supersedes() {
        let o = named("uid-a");
        o.observe(&[bound("w0", "uid-a")], true);
        assert_eq!(o.observe(&[Peer::new("w0", "w0")], true), None, "read by the ordinal");
        let unnamed = SelfObserver::new("w0", None);
        unnamed.enable();
        unnamed.observe(&[bound("w0", "uid-a")], true);
        assert_eq!(unnamed.observe(&[bound("w0", "uid-b")], true), None);
        assert_eq!(unnamed.current(), SelfEndpoint::Routable);
    }
}
