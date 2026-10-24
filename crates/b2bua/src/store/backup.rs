//! Which backup a call's writes go to, and what a turn's held messages wait
//! for (ADR-0055 §3): a new call is admitted with no backup, for life, while
//! the backlog toward the backup the proxy named is over its high-water mark,
//! and a call whose write cannot reach its backup has none from then on;
//! the ACK and the first provisional of an INVITE wait for the backup to apply
//! the call's current change-log entry, when it has a live one.

use repl_net::frame::{Partition, Watermark};

use super::{CallState, CallTopology, PropagateDirection, PutOpts};

/// What a turn's held message waits for: `peer` applying `at` on `partition`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldTarget {
    pub peer: String,
    pub partition: Partition,
    pub at: Watermark,
}

impl CallState {
    /// Admit the new call `call_ref` with the backup its `topology` names, or
    /// with none for its whole life when the backlog toward that backup is
    /// over its high-water mark. Called before the call's first write. `true`
    /// when the call has a backup.
    pub fn admit_backup(&self, call_ref: &str, topology: Option<&CallTopology>) -> bool {
        let Some(repl) = self.repl_store() else { return false };
        let Some(bak) = topology.map(|t| t.bak.as_str()).filter(|b| !b.is_empty()) else {
            return false;
        };
        if repl.changelog().admits_backup(bak) {
            repl.admit_backed(call_ref);
            return true;
        }
        repl.leave_unbacked(call_ref);
        self.metrics.bump_repl_new_call_unbacked();
        false
    }

    /// A write of `call_ref` toward `opts`' backup cannot reach it (its body
    /// does not encode): the call has no backup from here on, never a backup
    /// silently behind it.
    pub(super) fn abandon_backup(&self, opts: &PutOpts, call_ref: &str) {
        let (Some(repl), Some(peer)) = (self.repl_store(), &opts.peer) else { return };
        if opts.direction == Some(PropagateDirection::Forward) {
            repl.abandon_backup(peer, call_ref);
        }
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

    /// Resolves once the call's backup applied the call's current change-log
    /// entry, re-read on every wake: a later write of the call moves its entry
    /// past the position awaited, and the wait goes on to the new one, so
    /// another call's applied write never stands for this one. Resolves at
    /// once when the call has no live, bootstrapped backup or no entry toward
    /// it, and when the backup's log is gone.
    pub async fn applied_current(&self, call_ref: &str) {
        let Some(repl) = self.repl_store() else { return };
        let mut awaited = None;
        while let Some(target) = self.hold_target(call_ref) {
            if awaited == Some(target.at) {
                return;
            }
            repl.changelog().applied_reaches(&target.peer, target.partition, target.at).await;
            awaited = Some(target.at);
        }
    }
}
