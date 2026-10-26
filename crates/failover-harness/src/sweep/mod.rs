//! The failover sweep (ADR-0055 §11): every shape of a catalogue is cut at
//! every turn point of its call — each turn's write handed to replication,
//! its effects left, its write applied at the backup — by each [`Incident`].
//! Every cell must end with an RFC-clean, fully terminated call the parties
//! judge as their play declares, one CDR and nothing left on any node or in
//! the limiter; once the incident strikes, a clean end is one (§1, §4), and an
//! incident that loses the call states what the parties are left with instead
//! ([`Incident::loses_the_call`]); the copies of a split that never heals may
//! each write a CDR or none, the two copies of a split that heals, a killed
//! primary and the node that ends the call after it each one, never none
//! between them, and a call with no backup at most one (ADR-0055 §5). A kill
//! inside
//! the replication window declares its windows: a copy may re-use, once per
//! dialog, an in-dialog CSeq the dead primary sent before its backup applied
//! the write — so may the node answering a response of a call that had no
//! backup — and may send a reliable provisional whose RSeq cannot continue
//! the dead primary's. After a kill, the copy may CANCEL once a callee INVITE
//! the dead primary saw finally answered. Every kill of the primary and a
//! healed split accept the §4 clean end's BYE into a dialog the other owner
//! ended, closed by its 481. After a stall, the copy's repeat of the caller's
//! 2xx may differ from the dead owner's in its own Contact alone. A split that
//! never heals is judged on leaks, cleanup and the CDR count, never on the
//! parties' verdict: the two copies' degraded-mode findings are accepted
//! there alone (ADR-0055 §11).
//!
//! A cell is named `<shape>/<incident>/t<k>-<point>`. The cells that fail
//! before their correction lands are listed in a [`Ratchet`] with the decision
//! that owns them; a lane fails on a failing cell the ratchet does not list
//! and on a listed cell that passes.
//!
//! A consumer adds shapes with nothing but the shape: a [`SweepShape`] states
//! how the cluster decides the call and what the parties play, and
//! [`failover_sweep!`](crate::failover_sweep) expands a list of shapes into
//! the lane's tests against the consumer's own ratchet.

pub mod catalogue;
mod cell;
mod contact_repeat;
mod outcome;
mod parties;
mod ratchet;
mod unseen_rseq;

use b2bua::cdr::CdrRecord;
use scenario_harness::actor::ActorScenario;
use scenario_harness::realcall::CallEnv;

pub use cell::{run_cell, Cut, PRIMARY};
pub use parties::{Parties, ALICE, BOB, BOB2, CHARLIE, MEDIA, MEDIA_ROLE};
pub use ratchet::Ratchet;

use crate::cluster::ClusterRecipe;
use crate::turn_points::{TurnAt, TurnPoint};

/// A call shape the sweep cuts.
pub trait SweepShape: Send + Sync {
    /// The shape's name: the first segment of its cells' names.
    fn id(&self) -> &'static str;

    /// How the cluster's workers are built: by default every call is routed
    /// to [`BOB`] holding one limiter set.
    fn recipe(&self) -> ClusterRecipe {
        ClusterRecipe::routing_to(BOB.parse().expect("bob's address"))
    }

    /// What the parties play.
    fn scenario(&self) -> Box<dyn ActorScenario>;

    /// The callee roles that share [`BOB`]'s address, as `(role, prefix)`
    /// pairs: a role owns the legs whose Request-URI user part starts with
    /// one of its prefixes, the longest match winning. A shape whose decision
    /// routes every leg to one address names its roles here; each is bound in
    /// the call's environment, and bob is the first. Empty, the default,
    /// binds bob alone.
    fn shared_callees(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Adjust the call's environment (its dwells) before the parties play.
    fn call_env(&self, _env: &mut CallEnv<'_>) {}

    /// Whether the shape is cut by a split that never heals
    /// ([`Incident::Split`]): a representative few are, to check that the
    /// degraded mode leaks nothing.
    fn splits_unhealed(&self) -> bool {
        false
    }

    /// Check each CDR the call left, beyond their count: the dead owner's and
    /// the survivor's alike.
    fn check_cdr(&self, _cdr: &CdrRecord) -> Result<(), String> {
        Ok(())
    }
}

/// What befalls the call's owners at the armed turn point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Incident {
    /// The primary dies; the proxy and the backup see it gone.
    PrimaryKilled,
    /// The backup dies; the proxy and the primary see it gone.
    BackupKilled,
    /// The replication between the two owners is cut and the proxy reads the
    /// primary dead while it runs on; nothing heals.
    Split,
    /// A [`Split`](Self::Split) healed one second later.
    SplitHealed,
    /// The replication from the primary to the backup stalls for longer than
    /// the hold's cap, then the primary dies (ADR-0055 §3).
    StalledKilled,
    /// The call's backup refused its replica at its memory ceiling, so the
    /// call has no backup; the primary dies (ADR-0055 §3).
    UnbackedKilled,
}

impl Incident {
    /// Every incident, in lane order.
    pub const ALL: [Incident; 6] = [
        Incident::PrimaryKilled,
        Incident::BackupKilled,
        Incident::Split,
        Incident::SplitHealed,
        Incident::StalledKilled,
        Incident::UnbackedKilled,
    ];

    /// The incident's name in a cell's name.
    pub fn label(self) -> &'static str {
        match self {
            Incident::PrimaryKilled => "primary-killed",
            Incident::BackupKilled => "backup-killed",
            Incident::Split => "split",
            Incident::SplitHealed => "split-healed",
            Incident::StalledKilled => "stalled-killed",
            Incident::UnbackedKilled => "unbacked-killed",
        }
    }

    /// Whether the incident may lose the call as a non-HA B2BUA loses it
    /// (ADR-0055 §11): a caller whose INVITE drew nothing from a copy is
    /// answered by its own Timer B, a callee whose dialog no copy holds keeps
    /// it until its own timers end it. A stalled call leaves its primary's
    /// CDR, the one of the node that ends it after, or both, never none; an
    /// unbacked one at most one. Nothing is left on any node or in the limiter
    /// either way.
    pub fn loses_the_call(self) -> bool {
        matches!(self, Incident::StalledKilled | Incident::UnbackedKilled)
    }

    /// Whether the incident is cut at a turn's applied sub-point: a call with
    /// no backup has none, and a stall at it is a kill after the write.
    fn cuts_applied(self) -> bool {
        !self.loses_the_call()
    }
}

/// The name of a cell: `<shape>/<incident>/t<k>-<point>`.
pub fn cell_name(shape: &str, incident: Incident, at: TurnAt) -> String {
    format!("{shape}/{}/{}", incident.label(), at.label())
}

/// Run `shape` with no incident: the call must pass every check and take at
/// least one turn. Its turns are the sweep's turn points.
pub fn baseline(shape: &dyn SweepShape) -> Vec<crate::turn_points::Turn> {
    let name = format!("{}/baseline", shape.id());
    let turns = in_cell(&name, || run_cell(shape, &name, None, false))
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    assert!(!turns.is_empty(), "[{name}] the call took no turn at its primary");
    turns
}

/// Cut `shape` at every turn point of its baseline by `incident`, and hold
/// the failing cells to `ratchet`.
pub fn sweep(shape: &dyn SweepShape, incident: Incident, ratchet: &Ratchet) {
    let prefix = format!("{}/{}/", shape.id(), incident.label());
    let mut ran = Vec::new();
    let mut failed = std::collections::BTreeMap::new();
    if incident == Incident::Split && !shape.splits_unhealed() {
        eprintln!("{prefix}: no cell, the shape is not cut by a split that never heals");
        ratchet.hold(&prefix, &ran, &failed);
        return;
    }
    let turns = baseline(shape);
    for (i, turn) in turns.iter().enumerate() {
        let points =
            turn.points().iter().filter(|p| incident.cuts_applied() || **p != TurnPoint::Applied);
        for &point in points {
            let at = TurnAt { turn: i + 1, point };
            let name = cell_name(shape.id(), incident, at);
            let cut = Some(Cut { at, incident });
            if let Err(payload) = in_cell(&name, || run_cell(shape, &name, cut, false)) {
                failed.insert(name.clone(), panic_text(payload.as_ref()));
            }
            ran.push(name);
        }
    }
    eprintln!("{prefix}: {} cells, {} failing", ran.len(), failed.len());
    ratchet.hold(&prefix, &ran, &failed);
}

/// Run the one cell of `shape` cut at `at` by `incident`, its unified report
/// kept under `target/seq-reports/`: the reproduction of a failing cell.
/// Panics on any finding.
pub fn replay(shape: &dyn SweepShape, incident: Incident, at: TurnAt) {
    let name = cell_name(shape.id(), incident, at);
    in_cell(&name, || run_cell(shape, &name, Some(Cut { at, incident }), true))
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
}

/// The stack a cell runs on: a whole cluster's futures nest deep.
const CELL_STACK: usize = 256 * 1024 * 1024;

/// Drive the future `cell` builds to its end on a thread of its own, on a
/// fresh current-thread runtime whose clock is paused and whose random source
/// is seeded by the cell's name: a cell's tasks die with its runtime, every
/// unbiased `select!` draws alike on every run, and its panic is handed back.
fn in_cell<F: std::future::Future>(
    name: &str,
    cell: impl FnOnce() -> F + Send,
) -> std::thread::Result<F::Output>
where
    F::Output: Send,
{
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("sweep-cell".into())
            .stack_size(CELL_STACK)
            .spawn_scoped(scope, || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .start_paused(true)
                    .rng_seed(tokio::runtime::RngSeed::from_bytes(name.as_bytes()))
                    .build()
                    .expect("a paused runtime")
                    .block_on(Box::pin(cell()))
            })
            .expect("a cell thread")
            .join()
    })
}

/// The text a panic carried.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "a panic without a message".to_string()
    }
}

/// Expand shapes into the sweep lane's tests: per shape, its baseline and one
/// test per [`Incident`], each holding its failing cells to the ratchet at
/// `$ratchet` (a path). The tests live under a `failover_sweep` module, the
/// name the lane selects them by.
///
/// ```ignore
/// failover_harness::failover_sweep! {
///     ratchet: concat!(env!("CARGO_MANIFEST_DIR"), "/failover-sweep.ratchet");
///     reinvite => failover_harness::sweep::catalogue::Reinvite,
/// }
/// ```
#[macro_export]
macro_rules! failover_sweep {
    (ratchet: $ratchet:expr; $( $name:ident => $shape:expr ),+ $(,)?) => {
        mod failover_sweep {
            #[allow(unused_imports)]
            use super::*;
            $(
                mod $name {
                    #[allow(unused_imports)]
                    use super::*;
                    use $crate::sweep::{Incident, Ratchet, SweepShape};

                    fn shape() -> Box<dyn SweepShape> {
                        Box::new($shape)
                    }

                    fn run(incident: Incident) {
                        $crate::sweep::sweep(&*shape(), incident, &Ratchet::load($ratchet));
                    }

                    #[test]
                    fn baseline() {
                        $crate::sweep::baseline(&*shape());
                    }

                    #[test]
                    fn primary_killed() {
                        run(Incident::PrimaryKilled);
                    }

                    #[test]
                    fn backup_killed() {
                        run(Incident::BackupKilled);
                    }

                    #[test]
                    fn split() {
                        run(Incident::Split);
                    }

                    #[test]
                    fn split_healed() {
                        run(Incident::SplitHealed);
                    }

                    #[test]
                    fn stalled_killed() {
                        run(Incident::StalledKilled);
                    }

                    #[test]
                    fn unbacked_killed() {
                        run(Incident::UnbackedKilled);
                    }
                }
            )+
        }
    };
}
