//! The worker's side of a stuck poll (ADR-0057 §6): the call a stuck task
//! serves, and its discharge before the process exits.

use std::time::Duration;

use repl_net::frame::Partition;

use super::B2buaCore;
use crate::repl::Reach;

/// How the discharge of a call a stuck poll holds ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StuckDischarge {
    /// The stuck task no longer runs a turn of the call: its turn went on and
    /// ended, so the call is left to the ordinary exit.
    TurnOver,
    /// The call is no longer resident here.
    Gone,
    /// The discharge did not end the call here.
    NotEnded,
    /// Discharged; no peer holds a copy of it.
    Unbacked,
    /// Discharged, and the peer holding its copy applied the call's last
    /// change toward it (the delete of a call this worker is primary for, the
    /// terminated body a takeover copy writes back), or the call's entry left
    /// the log toward it.
    BackupApplied,
    /// Discharged; the peer holding its copy had not applied that change by
    /// the bound, or its flow went down first.
    BackupBehind,
    /// The discharge did not land within the bound.
    TimedOut,
}

impl StuckDischarge {
    /// The outcome's name in a log line.
    pub fn label(self) -> &'static str {
        match self {
            StuckDischarge::TurnOver => "turn_over",
            StuckDischarge::Gone => "gone",
            StuckDischarge::NotEnded => "not_ended",
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

    /// Discharge `call_ref`, the call the stuck body `task` holds, off the
    /// wire, and wait for the peer holding its copy to apply the delete, all
    /// within `within`: best effort before the process exits. The body is
    /// fenced off first, so a turn of it that goes on lands nothing; a body
    /// that is no longer the call's in-flight one ended its turn, and the
    /// call is left as it stands.
    pub async fn discharge_stuck(
        &self,
        call_ref: &str,
        task: tokio::task::Id,
        within: Duration,
    ) -> StuckDischarge {
        let in_flight = self.ctx.dispatcher.in_flight();
        in_flight.fence(task);
        if in_flight.body_of(call_ref) != Some(task) {
            return StuckDischarge::TurnOver;
        }
        let deadline = tokio::time::Instant::now() + within;
        let holder = self.copy_holder(call_ref);
        let ctx = self.ctx.clone();
        let now_ms = ctx.clock.now_ms();
        let discharge = self.node.scope(crate::router::discharge_stuck(&ctx, call_ref, now_ms));
        match tokio::time::timeout_at(deadline, discharge).await {
            Err(_) => return StuckDischarge::TimedOut,
            Ok(false) if self.ctx.state.contains(call_ref) => return StuckDischarge::NotEnded,
            Ok(false) => return StuckDischarge::Gone,
            Ok(true) => {}
        }
        let (Some((peer, partition)), Some(repl)) = (holder, &self.repl_store) else {
            return StuckDischarge::Unbacked;
        };
        let log = repl.changelog();
        let Some(at) = log.position_of(&peer, partition, call_ref) else {
            return StuckDischarge::BackupApplied;
        };
        match tokio::time::timeout_at(deadline, log.applied_reaches(&peer, partition, at)).await {
            Ok(Reach::Applied) => StuckDischarge::BackupApplied,
            Ok(Reach::Lost) | Err(_) => StuckDischarge::BackupBehind,
        }
    }

    /// The peer holding the other copy of `call_ref` and the flow it pulls:
    /// the backup of a call this worker is primary for, while the call has
    /// one, or the primary of a takeover copy it serves.
    fn copy_holder(&self, call_ref: &str) -> Option<(String, Partition)> {
        let topology = self.ctx.state.topology(call_ref)?;
        if topology.pri == self.ctx.config.self_ordinal {
            let backup = self.ctx.state.backup_of(call_ref)?;
            return Some((backup, Partition::Bak));
        }
        (!topology.pri.is_empty()).then_some((topology.pri, Partition::Pri))
    }
}
