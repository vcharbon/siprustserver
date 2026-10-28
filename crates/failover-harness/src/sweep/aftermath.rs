//! What an incident still does to the call's owners after its strike, each
//! step at its instant: a split heals, a stalled primary dies, the proxy's
//! probe reads an unannounced kill dead, its endpoint reads not ready and is
//! later force-deleted, and a killed primary restarts and is published again
//! once ready. Every step is a harness verb acting through a real cause
//! (ADR-0055 §13).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::cluster::Cluster;
use crate::incident::Incident;
use crate::roles::Placement;
use crate::turn_points::Turn;
use crate::WorkerHealth;

/// How long a split lasts before it heals ([`Incident::SplitHealed`]).
const HEAL_AFTER: Duration = Duration::from_secs(1);
/// How long the replication stalls before the primary dies
/// ([`Incident::StalledKilled`]): past the hold's cap, and the §17.2.1 bound.
const STALL_FOR: Duration = Duration::from_millis(300);
/// How long the proxy's OPTIONS probe takes to read an unannounced kill dead:
/// two missed windows (ADR-0031 case 3a).
const PROBE_READS_DEAD_AFTER: Duration = Duration::from_secs(2);
/// How long an unannounced kill's endpoint reads ready (ADR-0031 case 3a, the
/// first ~40 s): then the node controller marks it not ready, in the slice.
const NOT_READY_AFTER: Duration = Duration::from_secs(40);
/// When the evicted pod of an unannounced kill is force-deleted and its
/// endpoint leaves the slice (ADR-0031 case 3b, past the ~5 min eviction).
const DEPARTS_AFTER: Duration = Duration::from_secs(300);
/// How long the orchestrator takes to restart a killed primary
/// ([`Incident::PrimaryRebooted`]).
const REBOOT_AFTER: Duration = Duration::from_secs(1);
/// How often a restarted primary is looked at until it is ready.
const READY_POLL: Duration = Duration::from_millis(100);
/// How long a restarted primary has to become ready.
const READY_WITHIN: Duration = Duration::from_secs(30);

/// One step of an aftermath.
#[derive(Clone, Copy, Debug)]
enum Step {
    Heal,
    KillAfterTheStall,
    ProbeReadsDead,
    NotReady,
    Depart,
    Reboot,
}

/// The steps an incident still owes the call, in the order they come.
#[derive(Default)]
pub(super) struct Aftermath {
    steps: VecDeque<(Instant, Step)>,
    /// A restarted primary not yet ready: its new address, when it is next
    /// looked at, and when it must be ready by.
    rebooted: Option<Rebooted>,
    /// The call's turns as they stood when its primary restarted: the dead
    /// incarnation's, each applied or not by then. A later turn, or a later
    /// write that holds one of these by its version, is the new incarnation's.
    dead_turns: Option<Vec<Turn>>,
}

#[derive(Clone, Copy)]
struct Rebooted {
    addr: SocketAddr,
    poll_at: Instant,
    ready_by: Instant,
}

impl Aftermath {
    /// The steps `incident`, struck now, owes.
    pub(super) fn of(incident: Incident) -> Self {
        let now = Instant::now();
        let steps: Vec<(Duration, Step)> = match incident {
            Incident::SplitHealed => vec![(HEAL_AFTER, Step::Heal)],
            Incident::StalledKilled => vec![(STALL_FOR, Step::KillAfterTheStall)],
            Incident::PrimaryKilledUnannounced => {
                vec![
                    (PROBE_READS_DEAD_AFTER, Step::ProbeReadsDead),
                    (NOT_READY_AFTER, Step::NotReady),
                    (DEPARTS_AFTER, Step::Depart),
                ]
            }
            Incident::PrimaryRebooted => vec![(REBOOT_AFTER, Step::Reboot)],
            Incident::PrimaryKilled
            | Incident::BackupKilled
            | Incident::Split
            | Incident::UnbackedKilled => vec![],
        };
        Self {
            steps: steps.into_iter().map(|(after, s)| (now + after, s)).collect(),
            rebooted: None,
            dead_turns: None,
        }
    }

    /// The call's turns as they stood when its primary restarted, once it
    /// did: the dead incarnation's.
    pub(super) fn dead_turns(&self) -> Option<&[Turn]> {
        self.dead_turns.as_deref()
    }

    /// Whether a step is still owed.
    pub(super) fn pending(&self) -> bool {
        !self.steps.is_empty() || self.rebooted.is_some()
    }

    /// When the next step is due: a restarted primary is looked at every
    /// [`READY_POLL`].
    pub(super) fn next_at(&self) -> Instant {
        let poll = self.rebooted.map(|r| r.poll_at);
        let step = self.steps.front().map(|(at, _)| *at);
        match (poll, step) {
            (Some(p), Some(s)) => p.min(s),
            (p, s) => p.or(s).unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        }
    }

    /// Take every step due now, then publish a restarted primary that is
    /// ready: the proxy reads it alive. Panics when it is not ready within
    /// [`READY_WITHIN`].
    pub(super) async fn act(&mut self, cluster: &mut Cluster, placement: &Placement) {
        let primary = placement.primary.as_str();
        while let Some(&(at, step)) = self.steps.front() {
            if at > Instant::now() {
                break;
            }
            self.steps.pop_front();
            match step {
                Step::Heal => cluster.heal(placement),
                Step::KillAfterTheStall => {
                    cluster.fh.strike(Incident::PrimaryKilled);
                    cluster.kill(primary, &placement.backup);
                }
                Step::ProbeReadsDead => {
                    cluster.fh.mark(primary, None, "health", "the proxy's probe reads it dead");
                    cluster.proxy.set_health(primary, WorkerHealth::Dead);
                }
                Step::NotReady => cluster.fh.flap_not_ready(primary),
                Step::Depart => cluster.fh.depart(primary),
                Step::Reboot => {
                    let taken = cluster.fh.turn_points().first_call().map(|(_, t)| t);
                    self.dead_turns = Some(taken.unwrap_or_default());
                    let addr = cluster.reboot(primary).await;
                    let now = Instant::now();
                    self.rebooted =
                        Some(Rebooted { addr, poll_at: now, ready_by: now + READY_WITHIN });
                }
            }
        }
        let Some(rebooted) = self.rebooted.as_mut() else { return };
        if cluster.node(primary).is_ready() {
            let addr = rebooted.addr;
            self.rebooted = None;
            cluster.fh.readmit(primary, addr);
            cluster.fh.mark(primary, None, "health", "the proxy's probe reads it alive");
            cluster.proxy.set_health(primary, WorkerHealth::Alive);
        } else if Instant::now() >= rebooted.ready_by {
            panic!("the restarted primary {primary} is not ready within {READY_WITHIN:?}");
        } else {
            rebooted.poll_at = Instant::now() + READY_POLL;
        }
    }
}
