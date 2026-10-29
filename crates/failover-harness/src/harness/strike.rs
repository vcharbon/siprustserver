//! A failover test strikes an incident and is given that incident's rows of
//! the window table (ADR-0055 §11): the RFC windows and dialog-verdict
//! waivers open from the strike on, and the run's measured tier decides, at
//! its end, which of the rows that fired it accepts.

use std::sync::atomic::Ordering;

use b2bua::repl::SyncLevel;
use scenario_harness::WaiverScope;

use super::{FailoverHarness, HarnessHandle};
use crate::turn_points::Turn;
use crate::windows::tier::{measure, Measured};
use crate::windows::{self, Kind, Row, Strike, Tier};

/// One strike of a run.
pub(super) struct Struck {
    pub(super) strike: Strike,
    /// Every call's turns at its primary at the strike.
    pub(super) calls: Vec<(String, Vec<Turn>)>,
}

/// A row that accepted something this run, and what.
#[derive(Clone)]
pub struct Fired {
    pub row: &'static Row,
    pub what: String,
}

impl FailoverHarness {
    /// Run every worker spawned from now on at the replication sync `level`.
    /// The default is the one `B2BUA_REPL_SYNC` names, else the write path's.
    pub fn with_sync_level(mut self, level: SyncLevel) -> Self {
        self.level = level;
        self
    }

    /// The sync level the workers run at.
    pub fn sync_level(&self) -> SyncLevel {
        self.level
    }

    /// `strike` befalls the call now: open the RFC windows and dialog-verdict
    /// waivers of every row it opens at this run's level, on every party the
    /// run judges. The party outcome, goal drop and CDR rows are the sweep's
    /// to apply. A test strikes the incident it causes; it never picks a
    /// window.
    pub fn strike(&mut self, strike: impl Into<Strike>) {
        let strike = strike.into();
        let from = self.recorded_seq_high_water();
        let parties = self.harness.judged_parties();
        for row in windows::opened_by(strike, self.level) {
            match row.kind {
                Kind::Rfc { .. } => self.rfc_acceptance.open_row(row, from),
                Kind::Dialog { rule, methods, once_per_dialog } => {
                    for party in &parties {
                        for scope in dialog_scopes(row, rule, methods, once_per_dialog, party) {
                            self.harness.waive(scope.clone());
                            self.dialog_rows.push((scope, row));
                        }
                    }
                }
                Kind::Outcome(_) | Kind::GoalDrop | Kind::Cdr { .. } => {}
            }
        }
        let calls = self.turn_points.calls();
        self.struck.push(Struck { strike, calls });
    }

    /// The struck incident is over: every RFC window of a row that closes
    /// then stops accepting, from this instant on.
    pub fn incident_over(&mut self) {
        let until = self.recorded_seq_high_water();
        self.rfc_acceptance.close_over(until);
    }

    /// The tier this run stands in, measured now: the weakest of its
    /// strikes'. `None` for a run that struck nothing.
    pub fn tier(&self) -> Option<Tier> {
        let at_end = self.turn_points.calls();
        let tampered = self.harness.tampered.load(Ordering::Relaxed);
        let bound_passed = self.turn_points.bound_passed();
        self.struck
            .iter()
            .map(|s| {
                let m = Measured {
                    level: self.level,
                    strike: s.strike,
                    at_strike: &s.calls,
                    at_end: &at_end,
                    tampered,
                    bound_passed,
                };
                measure(&m)
            })
            .max()
    }

    /// The RFC findings and dialog verdicts a row accepted this run, in the
    /// order they were found.
    pub fn fired_rows(&self) -> Vec<Fired> {
        let (_, accepted) = self.partition_rfc_findings();
        let rfc = accepted.into_iter().filter_map(|((rule, lane, detail), id)| {
            Some(Fired { row: windows::row(id)?, what: format!("[{lane}] {rule}: {detail}") })
        });
        let dialog =
            self.harness.waived_dialog_verdicts().into_iter().filter_map(|(party, v, w)| {
                let (_, row) = self.dialog_rows.iter().find(|(scope, _)| *scope == w)?;
                Some(Fired { row, what: format!("[{party}] {}: {v}", v.rule()) })
            });
        rfc.chain(dialog).collect()
    }

    /// The rows that fired this run although its measured tier keeps them
    /// closed, that follow a re-sent request in a run where no node sent one
    /// again, or that need two serving workers in a run where no dialog took
    /// a BYE from two, rendered one per line, or `None`.
    pub(super) fn tier_gate_failure(&self) -> Option<String> {
        let tier = self.tier()?;
        let resent = self.turn_points.resent();
        let reasked = self.turn_points.reasked();
        let fired = self.fired_rows();
        let two_servers = fired.iter().any(|f| f.row.needs_two_servers())
            && windows::two_servers::byes_from_two_workers(
                &self.sip_entries(),
                &self.all_worker_sip_addrs,
            );
        let closed: Vec<String> = fired
            .into_iter()
            .filter_map(|f| {
                let why = if !tier.opens(f.row) {
                    format!("closed in the {} tier", tier.label())
                } else if f.row.needs_resend() && !resent {
                    "closed: no node sent a request again".to_string()
                } else if f.row.needs_reask() && !reasked {
                    "closed: no node asked a routing decision again".to_string()
                } else if f.row.needs_two_servers() && !two_servers {
                    "closed: no dialog took a BYE from two workers".to_string()
                } else {
                    return None;
                };
                Some(format!("  • {} ({why}): {}", f.row.id, f.what))
            })
            .collect();
        (!closed.is_empty()).then(|| {
            format!(
                "[{}] the run stands in the {} tier, where these rows are closed:\n{}",
                self.name,
                tier.label(),
                closed.join("\n")
            )
        })
    }

    /// Record that the test rewrote what replication carries: a stalled,
    /// delayed or cut stream, a rewound or implanted replica. Such a run is
    /// never Guaranteed.
    pub(super) fn tamper(&self) {
        self.harness.tampered.store(true, Ordering::Relaxed);
    }
}

impl HarnessHandle {
    /// The parties whose dialogs the run judges.
    pub(super) fn judged_parties(&self) -> Vec<String> {
        let guard = self.inner.lock().unwrap();
        guard.as_ref().map(scenario_harness::Harness::judged_parties).unwrap_or_default()
    }

    /// Register a dialog-verdict waiver.
    pub(super) fn waive(&self, scope: scenario_harness::WaiverScope) {
        if let Some(harness) = self.inner.lock().unwrap().as_ref() {
            harness.waive(scope);
        }
    }

    /// The dialog verdicts a waiver covered, each with its party and the
    /// first waiver covering it.
    pub(super) fn waived_dialog_verdicts(
        &self,
    ) -> Vec<(String, scenario_harness::dialogs::DialogVerdict, scenario_harness::WaiverScope)>
    {
        let guard = self.inner.lock().unwrap();
        guard.as_ref().map(scenario_harness::Harness::waived_dialog_verdicts).unwrap_or_default()
    }
}

/// The dialog-verdict waivers a `Dialog` row opens on `party`: one per
/// method it names, a NOTIFY only when it ends its subscription.
fn dialog_scopes(
    row: &Row,
    rule: &str,
    methods: &[&str],
    once_per_dialog: bool,
    party: &str,
) -> Vec<WaiverScope> {
    let base = || {
        let scope = WaiverScope::rule(rule, format!("{}: {}", row.id, row.covers));
        let scope = scope.on_party(party).conditional();
        if once_per_dialog {
            scope.from_now_once_per_dialog()
        } else {
            scope.from_now()
        }
    };
    if methods.is_empty() {
        return vec![base()];
    }
    methods
        .iter()
        .map(|m| match *m {
            "NOTIFY" => base().on_method(*m).terminating(),
            _ => base().on_method(*m),
        })
        .collect()
}
