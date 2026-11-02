//! [`ConfigCheck`]: the one consistency check of a worker's final composed
//! configuration. A runner records its own relations into a check, then
//! [`RunnerBase::spawn`](crate::RunnerBase::spawn) adds the generic ones over
//! the [`B2buaDeps`] it is about to start and refuses the start when any is
//! violated, every violation in one message naming the variables involved.
//!
//! The generic relations, each documented beside its parameters:
//!
//! | relation | where |
//! |---|---|
//! | every worker tunable, its range and its order against the others (setup deadline under the INVITE bound, keepalive floor, reaper window, capacity ceilings, event-queue gate) | `B2buaConfig::validate` |
//! | `B2BUA_UDP_INGRESS_BRAKE_PCT` in 1..=100 | [`validate_ingress_brake_pct`](crate::validate_ingress_brake_pct) |
//! | `B2BUA_LOOP_STALL_MS` above `B2BUA_CALL_CONTROL_TIMEOUT_MS` + the lock-wait margin | `liveness_env` |
//! | `B2BUA_DRAIN_MIN_MS` ≤ `B2BUA_DRAIN_GRACE_MS` | `drain_env` |
//! | `B2BUA_QUEUE` ≥ [`MIN_INGRESS_QUEUE`]; `B2BUA_CONCURRENCY`, `B2BUA_CALL_CAP` ≥ 1 | here |
//! | with replication: the replica TTL against the keepalive round and the INVITE bound, the backup ceilings against the emergency ceilings | `ReplicaConfig::validate` |
//! | with replication: sync cap < ACK and CANCEL bounds < T1, the backlog marks | `WritePath::validate` |
//! | with replication: the verdict hold ≥ freshness window + Timer F | `VerdictBounds::validate` |
//!
//! The replication link bounds (idle timeout against the heartbeat, the
//! dead-peer TTL against a blip) and the CDR sink's bounds are relations of
//! one variable group no composition step sets; they are checked where their
//! grammar is read.

use b2bua::B2buaDeps;

use crate::{liveness_env, validate_ingress_brake_pct, RunnerEnv};

/// The least inbound queue, packets: what the default 4 MiB receive buffer
/// holds of 16 KiB datagrams, so one full buffer drained at once fits.
pub const MIN_INGRESS_QUEUE: usize = 256;

/// The violations of one configuration, collected across every relation so a
/// refusal names them all at once.
#[derive(Debug, Default)]
pub struct ConfigCheck {
    violations: Vec<String>,
}

impl ConfigCheck {
    /// An empty check.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a violation unless `holds`. `why` names every variable the
    /// relation involves, with its value.
    pub fn require(&mut self, holds: bool, why: impl FnOnce() -> String) {
        if !holds {
            self.violations.push(why());
        }
    }

    /// Records the violation a grouped check returns, if any.
    pub fn take(&mut self, result: Result<(), String>) {
        if let Err(why) = result {
            self.violations.push(why);
        }
    }

    /// `Err` naming every violation recorded, `Ok` when none was.
    pub fn into_result(self) -> Result<(), String> {
        match self.violations.as_slice() {
            [] => Ok(()),
            [one] => Err(format!("invalid configuration: {one}")),
            all => {
                Err(format!("invalid configuration, {} violations: {}", all.len(), all.join(" | ")))
            }
        }
    }

    /// Adds the generic relations of `env` and the final `deps`.
    pub(crate) fn generic(&mut self, env: &RunnerEnv, deps: &B2buaDeps) {
        let config = &deps.config;
        self.take(config.validate());
        self.take(validate_ingress_brake_pct(env.udp_ingress_brake_pct));
        let call_control =
            std::time::Duration::from_millis(config.call_control_timeout_ms.unsigned_abs());
        self.take(liveness_env::check_loop_stall(env.loop_stall, call_control));
        self.require(env.drain.floor <= env.drain.grace, || {
            format!(
                "B2BUA_DRAIN_MIN_MS={} > B2BUA_DRAIN_GRACE_MS={}: the floor of a caught-up exit \
                 cannot outlast the drain's ceiling",
                env.drain.floor.as_millis(),
                env.drain.grace.as_millis()
            )
        });
        self.require(env.queue_max >= MIN_INGRESS_QUEUE, || {
            format!(
                "B2BUA_QUEUE={} < {MIN_INGRESS_QUEUE}: the inbound queue would drop one burst",
                env.queue_max
            )
        });
        self.require(env.concurrency >= 1 && env.call_cap >= 1, || {
            format!(
                "B2BUA_CONCURRENCY={} / B2BUA_CALL_CAP={}: a worker with no handler or no call \
                 slot serves nothing",
                env.concurrency, env.call_cap
            )
        });
        if let Some(repl) = &deps.replication {
            self.take(repl.replica.validate(config));
            self.take(repl.write_path.validate().map_err(|why| {
                format!(
                    "B2BUA_REPL_SYNC_CAP_MS / B2BUA_REPL_ACK_BOUND_MS / \
                     B2BUA_REPL_CANCEL_BOUND_MS / B2BUA_REPL_BACKLOG_*: {why}"
                )
            }));
            self.take(repl.verdict.validate().map_err(|why| {
                format!("B2BUA_DEAD_VERDICT_HOLD_MS / B2BUA_DEAD_NOTICE_FRESH_MS: {why}")
            }));
        }
    }
}
