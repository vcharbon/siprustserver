//! The worker's side of a stuck poll (ADR-0057 §6): the call a stuck task
//! serves, and its discharge before the process exits.

use std::time::Duration;

use repl_net::frame::Partition;

use super::B2buaCore;

/// How often the discharge's wait reads the backup's flow.
const BACKUP_POLL: Duration = Duration::from_millis(10);

/// How the discharge of a call a stuck poll holds ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StuckDischarge {
    /// No such call is resident here.
    Gone,
    /// Discharged; no peer holds a copy of it.
    Unbacked,
    /// Discharged, and the peer holding its copy applied everything this
    /// worker logged for it, the delete included.
    BackupApplied,
    /// Discharged; the peer holding its copy had not applied the delete by
    /// the bound.
    BackupBehind,
    /// The discharge did not land within the bound.
    TimedOut,
}

impl StuckDischarge {
    /// The outcome's name in a log line.
    pub fn label(self) -> &'static str {
        match self {
            StuckDischarge::Gone => "gone",
            StuckDischarge::Unbacked => "unbacked",
            StuckDischarge::BackupApplied => "backup_applied",
            StuckDischarge::BackupBehind => "backup_behind",
            StuckDischarge::TimedOut => "timed_out",
        }
    }
}

impl B2buaCore {
    /// The call whose handler body is the task `task`, if any; an owned
    /// handle the poll watchdog names a stuck task's call with.
    pub fn call_of_task(&self) -> impl Fn(tokio::task::Id) -> Option<String> + Send + Sync {
        let in_flight = self.ctx.dispatcher.in_flight();
        move |task| in_flight.call_of(task)
    }

    /// Discharge `call_ref`, the call a stuck poll holds, off the wire, and
    /// wait for the peer holding its copy to apply the delete, all within
    /// `within`: best effort before the process exits.
    pub async fn discharge_stuck(&self, call_ref: &str, within: Duration) -> StuckDischarge {
        let deadline = tokio::time::Instant::now() + within;
        let holder = self.copy_holder(call_ref);
        let ctx = self.ctx.clone();
        let now_ms = ctx.clock.now_ms();
        let discharge = self.node.scope(crate::router::discharge_stuck(&ctx, call_ref, now_ms));
        match tokio::time::timeout_at(deadline, discharge).await {
            Err(_) => return StuckDischarge::TimedOut,
            Ok(false) => return StuckDischarge::Gone,
            Ok(true) => {}
        }
        let (Some((peer, partition)), Some(repl)) = (holder, &self.repl_store) else {
            return StuckDischarge::Unbacked;
        };
        loop {
            if repl.changelog().flow_caught_up(&peer, partition) {
                return StuckDischarge::BackupApplied;
            }
            if tokio::time::Instant::now() >= deadline {
                return StuckDischarge::BackupBehind;
            }
            tokio::time::sleep(BACKUP_POLL).await;
        }
    }

    /// The peer holding the other copy of `call_ref` and the flow it pulls:
    /// the backup of a call this worker is primary for, the primary of a
    /// takeover copy it serves. `None` when the call names no other worker.
    fn copy_holder(&self, call_ref: &str) -> Option<(String, Partition)> {
        let topology = self.ctx.state.topology(call_ref)?;
        let me = &self.ctx.config.self_ordinal;
        let (holder, partition) = if topology.pri == *me {
            (topology.bak, Partition::Bak)
        } else {
            (topology.pri, Partition::Pri)
        };
        (!holder.is_empty()).then_some((holder, partition))
    }
}
