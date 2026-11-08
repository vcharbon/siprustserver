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
//! | `B2BUA_LOOP_EXIT_MS` above `B2BUA_LOOP_STALL_MS` and `B2BUA_CALL_CONTROL_TIMEOUT_MS` + the lock-wait margin | `liveness_env` |
//! | `B2BUA_DRAIN_MIN_MS` ≤ `B2BUA_DRAIN_GRACE_MS` | `drain_env` |
//! | `B2BUA_QUEUE` ≥ [`MIN_INGRESS_QUEUE`]; `B2BUA_CONCURRENCY`, `B2BUA_CALL_CAP` ≥ 1 | here |
//! | with replication: the replica TTL against the keepalive round and the INVITE bound, the backup ceilings against the emergency ceilings | `ReplicaConfig::validate` |
//! | with replication: sync cap < ACK and CANCEL bounds < T1, the backlog marks | `WritePath::validate` |
//! | with replication: the verdict hold ≥ freshness window + Timer F | `VerdictBounds::validate` |
//! | with replication: `B2BUA_DEAD_VERDICT_HOLD_MS` > 2 × `B2BUA_REPL_IDLE_TIMEOUT_MS` + `B2BUA_REPL_LOOKUP_TIMEOUT_MS` + `B2BUA_REPL_CONNECT_TIMEOUT_MS` | here: a verdict stays held while the backup confirms it (ADR-0047 D1) |
//! | with replication: `B2BUA_REPL_BOOTSTRAP_HARD_TIMEOUT_MS` > 2 × `B2BUA_REPL_CONNECT_TIMEOUT_MS` + `B2BUA_REPL_BACKOFF_MAX_MS` | here: a cold flow refused or timed out once still makes a second connect before the timer reads its peer unreachable (ADR-0011 X5) |
//! | with replication: `B2BUA_REPL_OPEN_TIMEOUT_MS` ≥ `B2BUA_REPL_IDLE_TIMEOUT_MS`; `B2BUA_REPL_MAX_CONNECTIONS` ≥ 4 | here: ADR-0045 item 5 |
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

/// The least cap on inbound replication connections: one peer's two flows,
/// each with the connection it replaces not yet read down.
pub const MIN_REPL_CONNECTIONS: usize = 4;

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
        self.take(crate::runner_env::caps_env::check_caps(&config.caps));
        self.take(validate_ingress_brake_pct(env.udp_ingress_brake_pct));
        let call_control = config.call_control_timeout();
        self.take(liveness_env::check_loop_stall(env.loop_stall, call_control));
        self.take(liveness_env::check_loop_exit(env.loop_exit, env.loop_stall, call_control));
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
            self.accept_bounds(&repl.serve, &repl.puller);
            if let Some(settings) = &env.replication {
                self.confirmation_fits_the_hold(repl, settings);
            }
            self.bootstrap_outlasts_a_retry(&repl.puller);
        }
    }

    /// A cold flow's bootstrap hard timer outlasts one failed connect, the
    /// longest backoff and a second connect: a peer refused or slow once is
    /// tried again before the timer reads it unreachable and releases the
    /// readiness it holds (ADR-0011 X5).
    fn bootstrap_outlasts_a_retry(&mut self, puller: &b2bua::repl::PullerConfig) {
        let (hard, connect, backoff) =
            (puller.bootstrap_hard_timeout_ms, puller.connect_timeout_ms, puller.backoff_max_ms);
        let retry = connect.saturating_mul(2).saturating_add(backoff);
        self.require(hard > retry, || {
            format!(
                "B2BUA_REPL_BOOTSTRAP_HARD_TIMEOUT_MS={hard} is not above 2 × \
                 B2BUA_REPL_CONNECT_TIMEOUT_MS={connect} + B2BUA_REPL_BACKOFF_MAX_MS={backoff}: \
                 a peer slow to answer one connect would be read unreachable and a returning \
                 worker read ready without the calls it backs up"
            )
        });
    }

    /// The serve loop's accept bounds against the link (ADR-0045 item 5).
    fn accept_bounds(
        &mut self,
        serve: &b2bua::repl::ServeConfig,
        puller: &b2bua::repl::PullerConfig,
    ) {
        let open = sip_clock::wall::millis_u64(serve.open_timeout);
        let idle = puller.idle_timeout_ms;
        self.require(open >= idle, || {
            format!(
                "B2BUA_REPL_OPEN_TIMEOUT_MS={open} is below B2BUA_REPL_IDLE_TIMEOUT_MS={idle} \
                 (ADR-0045 item 5)"
            )
        });
        let cap = serve.max_connections;
        self.require(cap >= MIN_REPL_CONNECTIONS, || {
            format!("B2BUA_REPL_MAX_CONNECTIONS={cap} < {MIN_REPL_CONNECTIONS} (ADR-0045 item 5)")
        });
    }

    /// The longest a backup confirms a dead verdict itself, a frozen peer
    /// whose kernel takes the connect: one idle timeout to read the stream
    /// down, the peer's lookup, one connect, and one idle timeout more
    /// (ADR-0047 D1). A repeat of the verdict's notice inside it is the same
    /// verdict only while the hold lasts.
    fn confirmation_fits_the_hold(
        &mut self,
        repl: &b2bua::ReplicationSetup,
        settings: &crate::ReplicationSettings,
    ) {
        let idle = repl.puller.idle_timeout_ms;
        let connect = repl.puller.connect_timeout_ms;
        let lookup = sip_clock::wall::millis_u64(settings.link.lookup_timeout);
        let confirm = idle.saturating_mul(2).saturating_add(lookup).saturating_add(connect);
        let hold = repl.verdict.hold_ms;
        self.require(u64::try_from(hold).is_ok_and(|h| h > confirm), || {
            format!(
                "B2BUA_DEAD_VERDICT_HOLD_MS={hold} is not above 2 × B2BUA_REPL_IDLE_TIMEOUT_MS={idle} \
                 + B2BUA_REPL_LOOKUP_TIMEOUT_MS={lookup} + B2BUA_REPL_CONNECT_TIMEOUT_MS={connect}: \
                 a repeat of a notice would start a second confirmation of one verdict"
            )
        });
    }
}

#[cfg(test)]
mod tests;
