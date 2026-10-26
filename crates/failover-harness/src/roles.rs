//! The verbs a failover test steers one call by (ADR-0055 §13): place the call
//! on a named primary and backup, steer the parties' traffic to a named role
//! through a real cause, and name the cluster's nodes by their role in the
//! call. Steering never bypasses the proxy: it changes what the proxy or the
//! network sees, and the proxy routes.

use scenario_harness::Agent;

use crate::cluster::Cluster;
use crate::{ReplicatedB2buaSut, WorkerHealth};

/// A node's role in one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The worker the proxy placed the call on.
    Primary,
    /// The worker the call's cookie names to take it over.
    Backup,
    /// The backup while it serves a copy of the call taken over from the
    /// primary.
    Copy,
}

/// What steers traffic away from a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The proxy reads the node dead; the process runs on.
    Health,
    /// The node's process dies; the proxy and the node's peers see it gone.
    Kill,
    /// The node is cut off the signalling plane, and the proxy reads it dead.
    Partition,
}

/// Where one call is placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub primary: String,
    pub backup: String,
    /// The Call-ID the caller's next call carries.
    pub call_id: String,
}

impl Placement {
    /// The ordinal holding `role` (a copy runs on the backup).
    pub fn ordinal(&self, role: Role) -> &str {
        match role {
            Role::Primary => &self.primary,
            Role::Backup | Role::Copy => &self.backup,
        }
    }

    /// The ordinal holding the other role.
    pub fn other(&self, role: Role) -> &str {
        match role {
            Role::Primary => &self.backup,
            Role::Backup | Role::Copy => &self.primary,
        }
    }
}

impl Cluster {
    /// Make `caller`'s next call land on `primary`: pick the Call-ID the proxy
    /// places there under its current view and seed the caller's next Call-ID
    /// with it. The backup is the worker the proxy pairs with it. The call
    /// must be the next identifier the caller draws: the test checks the
    /// placement on the call's first request ([`Self::assert_placed`]).
    pub fn place_next_call(&self, caller: &Agent, primary: &str) -> Placement {
        let lands = |id: &str| self.proxy.placement_of(id).is_some_and(|(p, _)| p == primary);
        let call_id = caller.seed_next_call_id(lands);
        let (_, backup) = self.proxy.placement_of(&call_id).expect("a placed call has a primary");
        Placement { primary: primary.to_string(), backup, call_id }
    }

    /// Assert the call reached the B2BUA as `placement` says: its first
    /// relayed request carries the cookie naming the placed primary and backup.
    pub fn assert_placed(placement: &Placement, request: &sip_message::SipRequest) {
        let (primary, backup) = crate::worker_ordinals(request);
        assert_eq!(
            (primary.as_str(), backup.as_str()),
            (placement.primary.as_str(), placement.backup.as_str()),
            "the call landed off its placement"
        );
    }

    /// The node holding `role` in the placed call; a [`Role::Copy`] only while
    /// the backup serves the call.
    pub fn view(
        &self,
        placement: &Placement,
        role: Role,
        call_ref: &str,
    ) -> Option<&ReplicatedB2buaSut> {
        let node = self.node(placement.ordinal(role));
        match role {
            Role::Copy => node.serves(call_ref).then_some(node),
            Role::Primary | Role::Backup => Some(node),
        }
    }

    /// Steer the parties' next requests, and the responses to the requests the
    /// call's nodes sent, to `to` by `cause` acting on the other node: the
    /// proxy reads `to` alive and routes the call there, as the call's cookie
    /// names both owners.
    pub fn steer(&mut self, placement: &Placement, to: Role, cause: Cause) {
        let from = placement.other(to).to_string();
        let to = placement.ordinal(to).to_string();
        self.proxy.set_health(&to, WorkerHealth::Alive);
        match cause {
            Cause::Health => {
                self.fh.mark(&from, None, "steer", &format!("health: the proxy reads {from} dead"));
                self.proxy.set_health(&from, WorkerHealth::Dead);
            }
            Cause::Kill => self.kill(&from, &to),
            Cause::Partition => {
                let at = self.node(&from).sip_addr();
                self.fh.cut_signalling(&from, at);
                self.proxy.set_health(&from, WorkerHealth::Dead);
            }
        }
    }

    /// Undo a [`Cause::Health`] or [`Cause::Partition`] steering away from
    /// `role`: the node is reachable and the proxy reads it alive.
    pub fn restore(&mut self, placement: &Placement, role: Role) {
        let ordinal = placement.ordinal(role).to_string();
        let at = self.node(&ordinal).sip_addr();
        self.fh.restore_signalling(&ordinal, at);
        self.proxy.set_health(&ordinal, WorkerHealth::Alive);
    }

    /// Kill `victim`: its process dies with its memory, the proxy reads it
    /// dead, and `survivor` sees its endpoint leave membership.
    pub fn kill(&mut self, victim: &str, survivor: &str) {
        self.fh.mark(victim, None, "crash", "kill (abrupt)");
        self.node_mut(victim).crash();
        self.proxy.set_health(victim, WorkerHealth::Dead);
        self.node(survivor).simulate_peer_removed(victim);
    }

    /// Split the two owners of the placed call: the replication between them
    /// is cut and the proxy reads the primary dead, so the backup takes the
    /// call's traffic while the primary runs on. Two copies can then mint one
    /// in-dialog CSeq: the window is accepted from here (ADR-0055 §8).
    pub fn split(&mut self, placement: &Placement) {
        self.fh.accept_rfc_deviations_from_now(
            crate::RULE_CSEQ_IN_DIALOG_ORDER,
            "ADR-0055 §8: two copies of a split call mint one in-dialog CSeq",
        );
        self.fh.partition(&placement.primary, &placement.backup);
        self.proxy.set_health(&placement.primary, WorkerHealth::Dead);
    }

    /// Heal a [`split`](Self::split): the replication flows again, the proxy
    /// reads the primary alive, and the duplicate-CSeq window closes: one
    /// copy is left to number the call's requests.
    pub fn heal(&mut self, placement: &Placement) {
        self.fh.resume_rfc_gate(crate::RULE_CSEQ_IN_DIALOG_ORDER);
        self.fh.heal(&placement.primary, &placement.backup);
        self.proxy.set_health(&placement.primary, WorkerHealth::Alive);
    }
}
