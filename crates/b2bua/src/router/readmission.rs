//! The readmission gate (ADR-0055 §8): a node re-serves its own calls only
//! while its own endpoint reads ready in its membership, the slice the proxy
//! routes from. While it does not, the proxy holds no address for the node,
//! so a response to a request it sends on a reclaimed call goes to the call's
//! other owner, which reads it as evidence of a lost write and ends the call
//! (§4). The reclaim commands, and the refused reverse flushes that meet what
//! they reclaim, are held until the endpoint reads ready and then run in
//! arrival order; every other command passes. The gate follows the live
//! sighting: a booting process's first snapshot may still show the process it
//! replaces ready, and a readiness flap closes it again. A membership that
//! does not show the node itself holds nothing.

use tokio::sync::watch;

use super::ReplCommand;

/// Holds the reclaim commands of a node whose endpoint does not read ready.
pub(super) struct ReadmissionGate {
    /// Whether the node's own endpoint reads ready; `None` when the
    /// membership does not show it.
    ready: Option<watch::Receiver<bool>>,
    held: Vec<ReplCommand>,
}

impl ReadmissionGate {
    /// A gate over `ready`, the node's own endpoint as its membership shows
    /// it; `None` opens it for good.
    pub(super) fn new(ready: Option<watch::Receiver<bool>>) -> Self {
        Self { ready, held: Vec::new() }
    }

    /// `cmd` when it runs now: it reclaims nothing, or the gate is open and
    /// holds nothing older. Otherwise `cmd` is held, a repeated bulk reclaim
    /// once.
    pub(super) fn admit(&mut self, cmd: ReplCommand) -> Option<ReplCommand> {
        let reclaims = matches!(
            cmd,
            ReplCommand::ReclaimAll
                | ReplCommand::ReclaimCall(_)
                | ReplCommand::ReverseFlushRefused { .. }
        );
        if !reclaims || (self.held.is_empty() && self.is_open()) {
            return Some(cmd);
        }
        let repeated = matches!(cmd, ReplCommand::ReclaimAll)
            && self.held.iter().any(|c| matches!(c, ReplCommand::ReclaimAll));
        if !repeated {
            self.held.push(cmd);
        }
        None
    }

    /// The held commands, in arrival order, once the endpoint reads ready;
    /// pending while the gate holds nothing or stays closed.
    pub(super) async fn released(&mut self) -> Vec<ReplCommand> {
        if self.held.is_empty() {
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
            held = self.held.len(),
            "own endpoint reads ready; held reclaim released"
        );
        std::mem::take(&mut self.held)
    }

    fn is_open(&mut self) -> bool {
        self.ready.as_mut().is_none_or(|ready| *ready.borrow_and_update())
    }
}

#[cfg(test)]
mod tests;
