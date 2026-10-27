//! The failover sweep (ADR-0055 §11): every shape of a catalogue is cut at
//! every turn point of its call — each turn's write handed to replication,
//! its effects left, its write applied at the backup — by each [`Incident`],
//! at each replication sync level. Every cell must end with nothing left on
//! any node or in the limiter, an RFC-clean, fully terminated call the
//! parties judge as their play declares, and one CDR, or two when the primary
//! dies (§5). What an incident may cause beyond that is the window table's
//! ([`crate::windows`]): the cell strikes its incident on the harness, is
//! given the incident's rows, and its measured tier decides which of the
//! rows that fired it accepts — none in the Guaranteed tier.
//!
//! A cell is named `<shape>/<level>/<incident>/t<k>-<point>`. The cells that
//! fail before their correction lands are listed in a [`Ratchet`] with the
//! decision that owns them, beside the count of each test's Guaranteed cells;
//! a test fails on a failing cell the ratchet does not list, on a listed cell
//! that passes, and on a Guaranteed count other than the listed one. A row's
//! witness cell, an upstream cell, must fire it: the dead-window check.
//!
//! A consumer adds shapes with nothing but the shape: a [`SweepShape`] states
//! how the cluster decides the call and what the parties play, and
//! [`failover_sweep!`](crate::failover_sweep) expands a list of shapes into
//! one test per shape, incident and level against the consumer's own
//! ratchet. Each test writes what its cells measured to
//! `target/sweep-report/` ([`report`]).

mod answered;
pub mod catalogue;
mod cell;
mod parties;
mod ratchet;
pub mod report;
mod witness;

use std::sync::Mutex;

use b2bua::cdr::CdrRecord;
pub use b2bua::repl::SyncLevel;
use scenario_harness::actor::ActorScenario;
use scenario_harness::realcall::CallEnv;

pub use cell::{run_cell, CellReport, Cut, PRIMARY};
pub use parties::{Parties, ALICE, BOB, BOB2, CHARLIE, MEDIA, MEDIA_ROLE};
pub use ratchet::Ratchet;

use crate::cluster::ClusterRecipe;
pub use crate::incident::Incident;
use crate::turn_points::{TurnAt, TurnPoint};
use crate::windows::Tier;

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

/// A sync level's name in a cell's name.
pub fn level_label(level: SyncLevel) -> &'static str {
    match level {
        SyncLevel::Applied => "applied",
        SyncLevel::Off => "off",
    }
}

/// The name of a cell: `<shape>/<level>/<incident>/t<k>-<point>`.
pub fn cell_name(shape: &str, level: SyncLevel, incident: Incident, at: TurnAt) -> String {
    format!("{}{}", test_prefix(shape, level, incident), at.label())
}

/// The prefix of the cells one test runs: `<shape>/<level>/<incident>/`.
pub fn test_prefix(shape: &str, level: SyncLevel, incident: Incident) -> String {
    format!("{shape}/{}/{}/", level_label(level), incident.label())
}

/// Run `shape` with no incident at `level`: the call must pass every check
/// and take at least one turn. Its turns are the sweep's turn points.
pub fn baseline(shape: &dyn SweepShape, level: SyncLevel) -> Vec<crate::turn_points::Turn> {
    let name = format!("{}/{}/baseline", shape.id(), level_label(level));
    let sink = Mutex::default();
    let turns = in_cell(&name, || run_cell(shape, &name, None, level, false, &sink))
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    assert!(!turns.is_empty(), "[{name}] the call took no turn at its primary");
    turns
}

/// One cell the sweep ran: its name, what it measured, and its failure.
pub struct Ran {
    pub name: String,
    pub at: TurnAt,
    pub measured: CellReport,
    pub failure: Option<String>,
}

/// Cut `shape` at every turn point of its baseline by `incident` at `level`,
/// write what the cells measured to the sweep report, check the witness
/// cells of the rows, and hold the failing cells and the Guaranteed count to
/// `ratchet`.
pub fn sweep(shape: &dyn SweepShape, incident: Incident, level: SyncLevel, ratchet: &Ratchet) {
    let prefix = test_prefix(shape.id(), level, incident);
    let mut ran: Vec<Ran> = Vec::new();
    if incident == Incident::Split && !shape.splits_unhealed() {
        eprintln!("{prefix}: no cell, the shape is not cut by a split that never heals");
    } else {
        let turns = baseline(shape, level);
        for (i, turn) in turns.iter().enumerate() {
            let points = turn
                .points()
                .iter()
                .filter(|p| incident.cuts_applied() || **p != TurnPoint::Applied);
            for &point in points {
                let at = TurnAt { turn: i + 1, point };
                let name = cell_name(shape.id(), level, incident, at);
                let cut = Some(Cut { at, incident });
                let sink = Mutex::default();
                let outcome = in_cell(&name, || run_cell(shape, &name, cut, level, false, &sink));
                let failure = outcome.err().map(|payload| panic_text(payload.as_ref()));
                let measured = sink.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
                ran.push(Ran { name, at, measured, failure });
            }
        }
    }
    let guaranteed = ran.iter().filter(|r| r.measured.tier == Some(Tier::Guaranteed)).count();
    let failing = ran.iter().filter(|r| r.failure.is_some()).count();
    eprintln!("{prefix}: {} cells, {failing} failing, {guaranteed} guaranteed", ran.len());
    report::write(&prefix, &ran);
    let dead = witness::dead_windows(shape.id(), incident, level, &ran);
    ratchet.hold(&prefix, &ran, guaranteed, &dead);
}

/// Run the one cell of `shape` cut at `at` by `incident` at `level`, its
/// unified report kept under `target/seq-reports/`: the reproduction of a
/// failing cell. Panics on any finding.
pub fn replay(shape: &dyn SweepShape, incident: Incident, level: SyncLevel, at: TurnAt) {
    let name = cell_name(shape.id(), level, incident, at);
    let sink = Mutex::default();
    let cut = Some(Cut { at, incident });
    in_cell(&name, || run_cell(shape, &name, cut, level, true, &sink))
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

/// Expand shapes into the sweep's tests: per shape and sync level, its
/// baseline and one test per [`Incident`], each holding its cells to the
/// ratchet at `$ratchet` (a path). The tests live under a `failover_sweep`
/// module: `failover_sweep::<shape>::<level>::<incident>`.
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

                    fn shape() -> Box<dyn $crate::sweep::SweepShape> {
                        Box::new($shape)
                    }

                    $crate::failover_sweep!(@level applied, Applied, $ratchet);
                    $crate::failover_sweep!(@level off, Off, $ratchet);
                }
            )+
        }
    };
    (@level $level:ident, $variant:ident, $ratchet:expr) => {
        mod $level {
            use $crate::sweep::{Incident, Ratchet};

            const LEVEL: $crate::sweep::SyncLevel = $crate::sweep::SyncLevel::$variant;

            fn run(incident: Incident) {
                $crate::sweep::sweep(&*super::shape(), incident, LEVEL, &Ratchet::load($ratchet));
            }

            #[test]
            fn baseline() {
                $crate::sweep::baseline(&*super::shape(), LEVEL);
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
    };
}
