//! The readmission gate (ADR-0055 §8): a node serves its reclaimed calls only
//! while its own endpoint reads ready in its membership, the slice the proxy
//! routes from. While it does not and another worker does, the proxy routes
//! nothing to the node, so a response to a request it sends on a reclaimed call
//! goes to the call's other owner, which reads it as evidence of a lost write and
//! ends the call (§4). With no worker ready the proxy routes on the workers'
//! answers (ADR-0050 §11), and the gate holds all the same: the node cannot tell
//! the two apart. The bulk reclaim parks its calls at once, nothing armed; the gate
//! holds their arming, the straggler reclaims and the refused reverse flushes
//! that meet what they reclaim until the endpoint reads ready, then releases
//! the arming first and the commands in arrival order; every other command
//! passes. The gate follows the live sighting: a booting process's first
//! snapshot may still show the process it replaces ready, and a readiness
//! flap closes it again. A membership that does not show the node itself
//! holds nothing.
//!
//! The calls a bulk reclaim parked on turns of their own (`reclaim::deferred`)
//! are armed with the rest, each by a turn queued behind its park: the gate
//! keeps them with the arming, across every bulk pass until it is released.

use std::collections::BTreeSet;

use tokio::sync::watch;

use super::ReplCommand;

/// Holds the arming of parked calls and the reclaim commands of a node whose
/// endpoint does not read ready.
pub(super) struct ReadmissionGate {
    /// Whether the node's own endpoint reads ready; `None` when the
    /// membership does not show it.
    ready: Option<watch::Receiver<bool>>,
    /// Whether parked calls wait to be armed.
    arming: bool,
    /// The calls whose park a bulk reclaim deferred, to arm with the rest.
    deferred: BTreeSet<String>,
    held: Vec<ReplCommand>,
}

/// What the gate releases once the endpoint reads ready.
pub(super) struct Released {
    /// Arm the parked calls, before any command runs.
    pub(super) arm: bool,
    /// The deferred parks to arm with them ([`ReadmissionGate::defer`]).
    pub(super) deferred: Vec<String>,
    /// The held commands, in arrival order.
    pub(super) cmds: Vec<ReplCommand>,
}

impl ReadmissionGate {
    /// A gate over `ready`, the node's own endpoint as its membership shows
    /// it; `None` opens it for good.
    pub(super) fn new(ready: Option<watch::Receiver<bool>>) -> Self {
        Self { ready, arming: false, deferred: BTreeSet::new(), held: Vec::new() }
    }

    /// A bulk reclaim parked `call_refs` on turns of their own: each is armed
    /// with the next arming, by a turn queued behind its park.
    pub(super) fn defer(&mut self, call_refs: impl IntoIterator<Item = String>) {
        self.deferred.extend(call_refs);
    }

    /// `cmd` when it runs now: it reclaims nothing, or the gate is open and
    /// holds nothing older. Otherwise `cmd` is held.
    pub(super) fn admit(&mut self, cmd: ReplCommand) -> Option<ReplCommand> {
        let reclaims =
            matches!(cmd, ReplCommand::ReclaimCall(_) | ReplCommand::ReverseFlushRefused { .. });
        if !reclaims || (self.holds_nothing() && self.is_open()) {
            return Some(cmd);
        }
        self.held.push(cmd);
        None
    }

    /// The deferred parks to arm now with the calls a bulk reclaim just
    /// parked, when the gate is open and holds nothing. Otherwise their
    /// arming is held, and released ahead of every held command.
    pub(super) fn arm_now(&mut self) -> Option<Vec<String>> {
        if self.holds_nothing() && self.is_open() {
            return Some(std::mem::take(&mut self.deferred).into_iter().collect());
        }
        self.arming = true;
        None
    }

    /// What the gate held, once the endpoint reads ready; pending while the
    /// gate holds nothing or stays closed.
    pub(super) async fn released(&mut self) -> Released {
        if self.holds_nothing() {
            return std::future::pending().await;
        }
        while !self.is_open() {
            let Some(ready) = self.ready.as_mut() else { break };
            if ready.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
        tracing::info!(
            node = observe::node(),
            arming = self.arming,
            held = self.held.len(),
            "own endpoint reads ready; held reclaim released"
        );
        Released {
            arm: std::mem::take(&mut self.arming),
            deferred: std::mem::take(&mut self.deferred).into_iter().collect(),
            cmds: std::mem::take(&mut self.held),
        }
    }

    fn holds_nothing(&self) -> bool {
        !self.arming && self.held.is_empty()
    }

    fn is_open(&mut self) -> bool {
        self.ready.as_mut().is_none_or(|ready| *ready.borrow_and_update())
    }
}

#[cfg(test)]
mod tests;
