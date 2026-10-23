//! Which backup a call's writes go to, and what a turn's held messages wait
//! for (ADR-0055 §3): a new call is admitted with no backup, for life, while
//! the backlog toward the backup the proxy named is over its high-water mark;
//! the ACK and the first provisional of an INVITE wait for the backup to apply
//! the call's current change-log entry, when it has a live one.

use call::Call;
use repl_net::frame::{Partition, Watermark};

use super::{CallState, PropagateDirection};

/// What a turn's held message waits for: `peer` applying `at` on `partition`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldTarget {
    pub peer: String,
    pub partition: Partition,
    pub at: Watermark,
}

impl CallState {
    /// Admit the new call `call` with the backup its topology names, or with
    /// none for its whole life when the backlog toward that backup is over its
    /// high-water mark. Called before the call's first write. `true` when the
    /// call has a backup.
    pub fn admit_backup(&self, call: &Call) -> bool {
        let Some(repl) = self.repl_store() else { return false };
        let Some(bak) = call.topology.as_ref().map(|t| t.bak.as_str()).filter(|b| !b.is_empty())
        else {
            return false;
        };
        if repl.changelog().admits_backup(bak) {
            return true;
        }
        repl.leave_unbacked(&call.call_ref);
        self.metrics.bump_repl_new_call_unbacked();
        false
    }

    /// What a message the turn of `call_ref` holds waits for: its backup applying
    /// the call's current change-log entry. `None` when the call has no live,
    /// bootstrapped backup, or no entry toward it: the message leaves at once.
    pub fn hold_target(&self, call_ref: &str) -> Option<HoldTarget> {
        let repl = self.repl_store()?;
        let (_, _, opts) = self.store_target(call_ref);
        let peer = opts.peer?;
        let partition = match opts.direction {
            Some(PropagateDirection::Reverse) => Partition::Pri,
            _ => Partition::Bak,
        };
        let log = repl.changelog();
        if !log.flow_live(&peer, partition) {
            return None;
        }
        let at = log.position_of(&peer, partition, call_ref)?;
        Some(HoldTarget { peer, partition, at })
    }

    /// Resolves once `target` is reached: its peer applied the entry, or its
    /// log is gone. Resolves at once on an unwired node.
    pub async fn applied(&self, target: &HoldTarget) {
        if let Some(repl) = self.repl_store() {
            repl.changelog().applied_reaches(&target.peer, target.partition, target.at).await;
        }
    }
}
