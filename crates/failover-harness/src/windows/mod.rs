//! The window table (ADR-0055 §11): every deviation from a perfect call an
//! incident may cause, one named row each — an RFC-rule window, a
//! dialog-verdict waiver, a party outcome, a clean-end goal drop or a CDR
//! count — and the tier that opens it.
//!
//! A cell, or a hand-written test, is placed in a [`Tier`] by what it
//! measured ([`tier::measure`]): `Guaranteed` opens no row, `Degraded` opens
//! every row but the off-only ones, `Off` opens every row. A test never picks
//! a window: it strikes an incident ([`crate::FailoverHarness::strike`]) and
//! is given that incident's rows. A row is open in its own tier and every
//! weaker one; a finding two rows could take is taken by the first in table
//! order, and every accepted finding names its row.

pub(crate) mod copies;
pub mod outcome;
pub(crate) mod stall_cseq;
pub(crate) mod stall_repeat;
mod table;
pub mod tier;
pub(crate) mod two_servers;
pub(crate) mod unseen_rseq;

use b2bua::repl::SyncLevel;

pub use outcome::{CutFacts, OutcomeFn};
pub use table::TABLE;

use crate::incident::Incident;
use crate::rfc_acceptance::Admits;
use crate::turn_points::TurnPoint;

/// How much a cell may deviate from a perfect call, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Level `applied`, a kill of a call backed and bootstrapped at every
    /// turn up to the strike, no entry hitting the cap: no row opens.
    Guaranteed,
    /// Level `applied`, any other case.
    Degraded,
    /// Level `off`.
    Off,
}

impl Tier {
    /// The tier's name in a report.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Guaranteed => "guaranteed",
            Tier::Degraded => "degraded",
            Tier::Off => "off",
        }
    }

    /// Whether `row` is open in this tier: its own tier or a weaker one.
    pub fn opens(self, row: &Row) -> bool {
        self >= row.tier
    }

    /// The weakest tier a cell at `level` can stand in: the rows a strike
    /// opens before the cell's tier is measured.
    pub fn weakest_at(level: SyncLevel) -> Tier {
        match level {
            SyncLevel::Applied => Tier::Degraded,
            SyncLevel::Off => Tier::Off,
        }
    }
}

/// A hand-written test's fault that no [`Incident`] names yet. Each keeps the
/// windows its test opened, under rows of its own (`T-…`), until the sweep
/// gains the incident.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transitional {
    /// A departed primary returns and answers while its writes reach the
    /// backup copy late, after the copy's own deadline acted on the call.
    AnsweredInsideTheLag,
    /// A primary departs with its routing decision unreplicated, or after its
    /// caller's transaction completed, and the backup refuses the call.
    UndecidedDeparture,
}

/// What strikes a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Strike {
    /// An incident of the sweep. `in_window` is false only for a kill of the
    /// primary at an applied sub-point past which the primary wrote nothing:
    /// the backup holds everything the dead primary did.
    Incident { incident: Incident, in_window: bool },
    /// A hand-written test's fault no incident names.
    Transitional(Transitional),
}

impl From<Incident> for Strike {
    /// The incident, inside the replication window.
    fn from(incident: Incident) -> Strike {
        Strike::Incident { incident, in_window: true }
    }
}

impl From<Transitional> for Strike {
    fn from(fault: Transitional) -> Strike {
        Strike::Transitional(fault)
    }
}

impl Strike {
    /// The incident struck, if it is one.
    pub fn incident(self) -> Option<Incident> {
        match self {
            Strike::Incident { incident, .. } => Some(incident),
            Strike::Transitional(_) => None,
        }
    }
}

/// Which strikes open a row.
#[derive(Clone, Copy, Debug)]
pub enum Opens {
    /// The incidents of `always`, and those of `in_window` struck inside the
    /// replication window.
    Incidents { always: &'static [Incident], in_window: &'static [Incident] },
    /// A hand-written test's fault.
    Transitional(Transitional),
}

impl Opens {
    /// The incidents of `list`, inside the replication window or not.
    pub const fn any(list: &'static [Incident]) -> Opens {
        Opens::Incidents { always: list, in_window: &[] }
    }

    /// The incidents of `list`, struck inside the replication window.
    pub const fn in_window(list: &'static [Incident]) -> Opens {
        Opens::Incidents { always: &[], in_window: list }
    }

    fn by(self, strike: Strike) -> bool {
        match (self, strike) {
            (
                Opens::Incidents { always, in_window: listed },
                Strike::Incident { incident, in_window },
            ) => always.contains(&incident) || (in_window && listed.contains(&incident)),
            (Opens::Transitional(a), Strike::Transitional(b)) => a == b,
            _ => false,
        }
    }

    /// The incidents that open the row, inside the window or not.
    pub fn incidents(self) -> impl Iterator<Item = Incident> {
        let (always, in_window): (&[Incident], &[Incident]) = match self {
            Opens::Incidents { always, in_window } => (always, in_window),
            Opens::Transitional(_) => (&[], &[]),
        };
        always.iter().chain(in_window).copied()
    }
}

/// What a row accepts.
#[derive(Clone, Copy)]
pub enum Kind {
    /// Findings of `rules` on messages captured from the strike on, at most
    /// one offending request per dialog when `once_per_dialog`, only those
    /// `admits` passes when stated.
    Rfc { rules: &'static [&'static str], once_per_dialog: bool, admits: Option<Admits> },
    /// The `dialog-…` verdicts of `rule` each party records from the strike
    /// on, on `methods` (a terminating NOTIFY only, for `NOTIFY`) when stated,
    /// at most one per dialog when `once_per_dialog`.
    Dialog { rule: &'static str, methods: &'static [&'static str], once_per_dialog: bool },
    /// A failed party verdict the predicate accepts.
    Outcome(OutcomeFn),
    /// A party the system under test ended cleanly drops her remaining goals.
    GoalDrop,
    /// A CDR count outside the incident's own ([`Incident::cdrs`]), inside
    /// this range.
    Cdr { min: usize, max: usize },
}

/// One row of the window table.
pub struct Row {
    /// The row's stable id, the backticked id of its ADR-0055 §11 bullet
    /// (`W-…`), or a transitional row's (`T-…`).
    pub id: &'static str,
    /// The strongest tier the row opens in; it is open in every weaker one.
    /// Never [`Tier::Guaranteed`].
    pub tier: Tier,
    /// The strikes that open it.
    pub opens: Opens,
    /// What it covers, in a sentence.
    pub covers: &'static str,
    pub kind: Kind,
    /// An RFC window or dialog waiver closes when the test declares the
    /// incident over ([`crate::FailoverHarness::incident_over`]); every other
    /// one stays open to the end of the run.
    pub closes_when_over: bool,
    /// The upstream sweep cell the row fires in, checked by the test holding
    /// it; a transitional row names none.
    pub witness: Option<Witness>,
}

impl Row {
    /// Whether the row accepts only what follows a request a node sent again
    /// (`W-stale-resend…`): it stays closed in a run where none was.
    pub fn needs_resend(&self) -> bool {
        table::RESEND_ROWS.contains(&self.id)
    }

    /// Whether the row accepts only a dialog two workers each ended
    /// (`W-off-reboot-two-servers`): it stays closed in a run where no dialog
    /// took a BYE from two workers.
    pub fn needs_two_servers(&self) -> bool {
        table::TWO_SERVER_ROWS.contains(&self.id)
    }

    /// Whether `strike` opens this row in a cell at `level`, its tier not yet
    /// measured.
    pub fn opened_by(&self, strike: Strike, level: SyncLevel) -> bool {
        self.opens.by(strike) && Tier::weakest_at(level).opens(self)
    }
}

/// A sweep cell, by its shape, sync level, incident and cut sub-point: at
/// the level whose weakest tier is the row's own when the row fires there,
/// else at `off`.
#[derive(Clone, Copy, Debug)]
pub struct Witness {
    pub shape: &'static str,
    pub level: SyncLevel,
    pub incident: Incident,
    pub turn: usize,
    pub point: TurnPoint,
}

/// The row of `id`.
pub fn row(id: &str) -> Option<&'static Row> {
    TABLE.iter().find(|r| r.id == id)
}

/// The rows `strike` opens in a cell at `level`, in table order.
pub fn opened_by(strike: Strike, level: SyncLevel) -> impl Iterator<Item = &'static Row> {
    TABLE.iter().filter(move |r| r.opened_by(strike, level))
}
