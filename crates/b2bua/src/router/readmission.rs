//! The readmission gate (ADR-0055 §8): a node re-serves its own partition only
//! once it has observed its own endpoint routable. Before that the proxy holds
//! no address for it, so a response to a request it sends on a reclaimed call
//! goes to the call's other owner, which reads it as evidence of a lost write
//! and ends the call (§4). The reclaim commands are held until then and run in
//! arrival order; every other command passes. A membership that does not show
//! the node itself holds nothing.

use tokio::sync::watch;

use super::ReplCommand;
use crate::repl::SelfEndpoint;

/// Holds the reclaim commands of a node not yet observed routable.
pub(super) struct ReadmissionGate {
    /// The node's own endpoint; `None` when the membership does not show it.
    endpoint: Option<watch::Receiver<SelfEndpoint>>,
    /// Latched once the endpoint was observed routable: a later withdrawal or
    /// supersession closes nothing.
    open: bool,
    held: Vec<ReplCommand>,
}

impl ReadmissionGate {
    /// A gate over `endpoint`, the node's own endpoint as its membership shows
    /// it; `None` opens it for good.
    pub(super) fn new(endpoint: Option<watch::Receiver<SelfEndpoint>>) -> Self {
        let open = endpoint.is_none();
        Self { endpoint, open, held: Vec::new() }
    }

    /// `cmd` when it runs now: the gate is open, or `cmd` reclaims nothing.
    /// A reclaim command reaching a closed gate is held, a repeated bulk
    /// reclaim once.
    pub(super) fn admit(&mut self, cmd: ReplCommand) -> Option<ReplCommand> {
        if self.observe() || !matches!(cmd, ReplCommand::ReclaimAll | ReplCommand::ReclaimCall(_)) {
            return Some(cmd);
        }
        let repeated = matches!(cmd, ReplCommand::ReclaimAll)
            && self.held.iter().any(|c| matches!(c, ReplCommand::ReclaimAll));
        if !repeated {
            self.held.push(cmd);
        }
        None
    }

    /// The held commands, in arrival order, once the endpoint is observed
    /// routable; pending while the gate holds nothing or stays closed.
    pub(super) async fn released(&mut self) -> Vec<ReplCommand> {
        if self.held.is_empty() {
            return std::future::pending().await;
        }
        while !self.observe() {
            let Some(endpoint) = self.endpoint.as_mut() else { break };
            if endpoint.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
        tracing::info!(
            node = observe::node(),
            held = self.held.len(),
            "own endpoint routable; held reclaim released"
        );
        std::mem::take(&mut self.held)
    }

    /// Whether the gate is open, latching it on a routable sighting.
    fn observe(&mut self) -> bool {
        if !self.open {
            self.open = self.endpoint.as_mut().is_none_or(|endpoint| {
                matches!(
                    *endpoint.borrow_and_update(),
                    SelfEndpoint::Routable | SelfEndpoint::Withdrawn(_)
                )
            });
        }
        self.open
    }
}

#[cfg(test)]
mod tests;
