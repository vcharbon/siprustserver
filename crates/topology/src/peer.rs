//! [`Peer`] — one cluster member as a membership source lists it.

use crate::Ordinal;

/// A cluster member: identity ordinal + reachability host + the worker identity
/// bound to the ordinal + the orchestrator's endpoint conditions.
/// **Port-agnostic** — membership carries no transport port (that is a
/// consumer concern). Cf. the proxy's `WorkerEntry`, minus
/// `address.port`, `health`, and the LB timing stamps.
///
/// A member is **pullable** while it is in the snapshot at all, whatever its
/// conditions; it is **routable** only while `ready` (ADR-0031 D1). A condition
/// flip is a [`ConditionsChanged`](crate::MemberDelta::ConditionsChanged),
/// never a departure or a host move, so a consumer keyed on presence (the
/// replication supervisor) keeps its pullers and their watermarks across it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub ordinal: Ordinal,
    pub host: String,
    /// The worker identity the source binds to the ordinal: one boot of one
    /// worker (the orchestrator's pod UID). Compared for equality only; no
    /// order is drawn from it. `None` where the source names none (a static
    /// list).
    pub identity: Option<String>,
    /// The endpoint's `ready` condition: the member serves new traffic. An
    /// absent condition reads as not ready.
    pub ready: bool,
    /// The endpoint's `terminating` condition: the orchestrator has begun
    /// removing the member; it is still a replication peer until it leaves.
    pub terminating: bool,
}

impl Peer {
    /// A ready, non-terminating member — the steady-state shape.
    pub fn new(ordinal: impl Into<Ordinal>, host: impl Into<String>) -> Self {
        Self {
            ordinal: ordinal.into(),
            host: host.into(),
            identity: None,
            ready: true,
            terminating: false,
        }
    }

    /// A member with explicit endpoint conditions.
    pub fn with_conditions(
        ordinal: impl Into<Ordinal>,
        host: impl Into<String>,
        ready: bool,
        terminating: bool,
    ) -> Self {
        Self { ordinal: ordinal.into(), host: host.into(), identity: None, ready, terminating }
    }

    /// The same member bound to the worker `identity`.
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// The same member with `ready = false`.
    pub fn not_ready(mut self) -> Self {
        self.ready = false;
        self
    }

    /// The same member with `terminating = true`.
    pub fn terminating(mut self) -> Self {
        self.terminating = true;
        self
    }

    /// Whether `other` binds the ordinal elsewhere: another host or another
    /// worker identity.
    pub(crate) fn binding_differs(&self, other: &Peer) -> bool {
        self.host != other.host || self.identity != other.identity
    }

    /// Whether `other` is the same member at the same host with different
    /// conditions.
    pub(crate) fn conditions_differ(&self, other: &Peer) -> bool {
        self.ready != other.ready || self.terminating != other.terminating
    }
}
