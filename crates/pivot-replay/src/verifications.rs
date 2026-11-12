//! A cell's verdict split into its verification lines (ADR-0060
//! appendix §8): `verifications.json` in the cell's bundle, the
//! `@sip/contracts` `Verification` line shape, one line per question the
//! replay answered in process.
//!
//! - `call`, the interpreter's: every failure no registered gate charged;
//! - each gate of the product's [`Gates`] table: a line once the lane stated
//!   how it stands on it ([`declare`]) or charged a failure to it; a gate the
//!   batch's end answers is `pending` until the end settles it ([`settle`]);
//! - the table's [`Reference`], informative: why a gate compared no reference;
//! - a lane's known bug ([`RunVerdict::waived`]) is a `waived` failure of the
//!   line its finding belongs to, an informative check one marked
//!   `informative`: neither decides the cell.
//!
//! A failing line's detail names the verdict failures charged to it
//! (`failures`, their indices in `verdict.json`). The file is rewritten from
//! the verdict on every [`crate::verdict::write`], so it never states a
//! failure the verdict lost nor misses one it gained; how the lane stands on
//! each gate is read back off the lines the file already holds.

use std::collections::BTreeMap;
use std::path::Path;

use pivot_schema::bundle::{Failure as Finding, RunVerdict, VerdictStatus};
use serde::{Deserialize, Serialize};

use crate::Failure;

/// The bundle file holding a cell's lines.
pub const FILE: &str = "verifications.json";

/// The interpreter's line: the call as the document states it.
pub const CALL: &str = "call";

/// The source of the `call` line.
const INTERPRETER: &str = "interpreter";

/// One gate a product registers: its line's name and source, and which
/// verdict failures its site charges to it.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub name: &'static str,
    pub source: &'static str,
    /// Whether `failure` is this gate's: a failure no gate claims is the call's.
    pub charges: fn(&Finding) -> bool,
}

/// The informative companion of a gate that compares against a reference: the
/// line stating why the reference was not compared, where it was not.
#[derive(Debug, Clone, Copy)]
pub struct Reference {
    pub name: &'static str,
    /// The gate whose comparison it qualifies; its source is the line's.
    pub gate: &'static str,
}

/// The gates a product registers, in the order a cell states them after `call`.
#[derive(Debug, Clone, Copy)]
pub struct Gates {
    pub gates: &'static [Gate],
    pub reference: Option<Reference>,
}

impl Gates {
    /// No gate: every failure is the call's.
    pub const NONE: Gates = Gates { gates: &[], reference: None };

    /// The gates' names, in their order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.gates.iter().map(|gate| gate.name)
    }

    /// The line `failure` belongs to: the first gate that charges it, else the call.
    pub fn line_of(&self, failure: &Finding) -> &'static str {
        self.gates.iter().find(|gate| (gate.charges)(failure)).map_or(CALL, |gate| gate.name)
    }

    /// Who answers the line `name`.
    fn source_of(&self, name: &str) -> &'static str {
        let gate = match self.reference {
            Some(reference) if reference.name == name => reference.gate,
            _ => name,
        };
        self.gates.iter().find(|g| g.name == gate).map_or(INTERPRETER, |g| g.source)
    }

    fn is_gate(&self, name: &str) -> bool {
        self.gates.iter().any(|gate| gate.name == name)
    }

    fn is_reference(&self, name: &str) -> bool {
        self.reference.is_some_and(|reference| reference.name == name)
    }
}

/// What a line answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Answer {
    Pass,
    Fail,
    NotCheckable,
    Pending,
}

/// How a lane stands on a gate it answers for a cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Answered in the cell: a pass unless a failure was charged to it.
    Judged,
    /// Answered by the batch's end: `pending` until [`settle`], a failure as
    /// soon as one is charged to it.
    Pending,
    /// Not checked, for the reason given: `not-checkable` unless a failure was
    /// charged to it.
    Unchecked(String),
}

/// Where a failing line points.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// The indices in `verdict.json`'s `failures` of the failures charged to the line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<usize>,
}

/// One verification line, as `@sip/contracts` `Verification` reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub name: String,
    pub source: String,
    pub answer: Answer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Detail>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub informative: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waived: bool,
}

/// The lines of one verdict under one gate table.
struct Writer<'g> {
    gates: &'g Gates,
}

impl Writer<'_> {
    /// A line of `name` with nothing to say beyond its answer.
    fn bare(&self, name: &str, answer: Answer, message: Option<String>) -> Line {
        Line {
            name: name.into(),
            source: self.gates.source_of(name).into(),
            answer,
            message,
            detail: None,
            informative: false,
            waived: false,
        }
    }

    /// What a failure states as a line's detail: its step, what was expected
    /// and what was seen, its side (the system under test's for a gate's,
    /// none for the call's), and where it sits in the verdict.
    fn detail_of(
        &self,
        name: &str,
        failure: &Finding,
        at: Option<usize>,
        failed_step: Option<&str>,
    ) -> Detail {
        let (expected, seen) = match failure {
            Finding::CheckFailed { expected, observed, .. }
            | Finding::CdrMismatch { expected, observed } => {
                (Some(expected.clone()), Some(observed.clone()))
            }
            _ => (None, None),
        };
        let step = failure.step().or(if name == CALL { failed_step } else { None });
        let side = (self.gates.line_of(failure) != CALL).then(|| "sut".to_string());
        Detail {
            step: step.map(str::to_string),
            expected,
            seen,
            side,
            failures: at.into_iter().collect(),
        }
    }

    /// The failing line `name` over `failures`, each with its index in the
    /// verdict where it has one: the first stated, and how many more.
    fn failed(
        &self,
        name: &str,
        failures: &[(Option<usize>, &Finding)],
        failed_step: Option<&str>,
    ) -> Option<Line> {
        let (at, first) = failures.first()?;
        let more = match failures.len() {
            1 => String::new(),
            n => format!(" (and {} more)", n - 1),
        };
        let mut detail = self.detail_of(name, first, *at, failed_step);
        detail.failures = failures.iter().filter_map(|(at, _)| *at).collect();
        Some(Line {
            detail: Some(detail),
            ..self.bare(name, Answer::Fail, Some(format!("{first}{more}")))
        })
    }

    /// The gate `gate` with no failure charged to it, as the lane stands on
    /// it; `reference` is why the gate's reference was not compared, where
    /// the gate has one and it was not.
    fn uncharged(&self, gate: &str, standing: &Standing, reference: Option<&str>) -> Line {
        match standing {
            Standing::Pending => self.bare(gate, Answer::Pending, None),
            Standing::Unchecked(why) => self.bare(gate, Answer::NotCheckable, Some(why.clone())),
            Standing::Judged => self.bare(
                gate,
                Answer::Pass,
                reference.map(|why| format!("the reference was not compared: {why}")),
            ),
        }
    }

    fn lines_of(&self, verdict: &RunVerdict, standings: &BTreeMap<String, Standing>) -> Vec<Line> {
        let failed_step = verdict.failed_step.as_deref();
        let charged = |name: &str| -> Vec<(Option<usize>, &Finding)> {
            verdict
                .failures
                .iter()
                .enumerate()
                .filter(|(_, failure)| self.gates.line_of(failure) == name)
                .map(|(at, failure)| (Some(at), failure))
                .collect()
        };
        let call = self.failed(CALL, &charged(CALL), failed_step).unwrap_or_else(|| {
            if verdict.status == VerdictStatus::Failed && verdict.failures.is_empty() {
                self.bare(
                    CALL,
                    Answer::Fail,
                    Some("the verdict failed and names no failure".into()),
                )
            } else {
                self.bare(CALL, Answer::Pass, None)
            }
        });
        let reference =
            self.gates.reference.and_then(|reference| match standings.get(reference.name) {
                Some(Standing::Unchecked(why)) => Some((reference, why.as_str())),
                _ => None,
            });
        let mut lines = vec![call];
        for gate in self.gates.gates {
            let unmatched = reference.filter(|(r, _)| r.gate == gate.name).map(|(_, why)| why);
            let line = match (
                self.failed(gate.name, &charged(gate.name), failed_step),
                standings.get(gate.name),
            ) {
                (Some(line), _) => line,
                (None, Some(standing)) => self.uncharged(gate.name, standing, unmatched),
                (None, None) => continue,
            };
            lines.push(line);
        }
        lines.extend(reference.map(|(reference, why)| Line {
            informative: true,
            ..self.bare(reference.name, Answer::NotCheckable, Some(why.to_string()))
        }));
        let marked = |finding: &Finding, message: String| {
            let name = self.gates.line_of(finding);
            Line {
                detail: Some(self.detail_of(name, finding, None, None)),
                ..self.bare(name, Answer::Fail, Some(message))
            }
        };
        lines.extend(verdict.waived.iter().map(|waived| Line {
            waived: true,
            ..marked(&waived.finding, format!("known bug {}: {}", waived.bug, waived.finding))
        }));
        lines.extend(verdict.informative.iter().map(|informative| Line {
            informative: true,
            ..marked(
                &informative.finding,
                format!("{}: {}", informative.class, informative.finding),
            )
        }));
        lines
    }
}

/// The lines of `verdict` under `gates`: `call` first, then each gate of
/// `standings` or charged in table order, the reference line where it stands
/// unchecked, then the waived and informative findings.
pub fn lines_of(
    verdict: &RunVerdict,
    standings: &BTreeMap<String, Standing>,
    gates: &Gates,
) -> Vec<Line> {
    Writer { gates }.lines_of(verdict, standings)
}

/// How the lane stands on each gate of `gates` the cell's `verifications.json`
/// already states, the reference included: a `pending` line pending, a
/// `not-checkable` one unchecked for its message, any other answered. Empty
/// where the file is absent or unreadable.
fn standings_in(out_dir: &Path, gates: &Gates) -> BTreeMap<String, Standing> {
    std::fs::read_to_string(out_dir.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Line>>(&text).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|line| {
            let gate = gates.is_gate(&line.name) && !line.informative;
            let reference = gates.is_reference(&line.name) && line.informative;
            !line.waived && (gate || reference)
        })
        .map(|line| {
            let standing = match line.answer {
                Answer::Pending => Standing::Pending,
                Answer::NotCheckable => Standing::Unchecked(line.message.unwrap_or_default()),
                Answer::Pass | Answer::Fail => Standing::Judged,
            };
            (line.name, standing)
        })
        .collect()
}

fn write(
    out_dir: &Path,
    verdict: &RunVerdict,
    standings: &BTreeMap<String, Standing>,
    gates: &Gates,
) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(&lines_of(verdict, standings, gates))
        .map_err(|e| Failure::Environment(format!("serialize {FILE}: {e}")))?;
    std::fs::write(out_dir.join(FILE), text + "\n")
        .map_err(|e| Failure::Environment(format!("write {FILE}: {e}")))
}

/// Rewrites the cell's lines from `verdict`, keeping how the lane stands on each gate.
pub fn rewrite(out_dir: &Path, verdict: &RunVerdict, gates: &Gates) -> Result<(), Failure> {
    write(out_dir, verdict, &standings_in(out_dir, gates), gates)
}

/// States how the lane stands on each of `standing` for this cell, over what
/// it stated before, and rewrites its lines from `verdict`.
pub fn declare(
    out_dir: &Path,
    verdict: &RunVerdict,
    standing: &[(&str, Standing)],
    gates: &Gates,
) -> Result<(), Failure> {
    let mut standings = standings_in(out_dir, gates);
    standings.extend(standing.iter().map(|(gate, standing)| (gate.to_string(), standing.clone())));
    write(out_dir, verdict, &standings, gates)
}

/// The batch's end answers each of `standing` still `pending` for the cell,
/// and rewrites its lines from the verdict its bundle holds; a gate already
/// answered keeps its answer. A cell with no lines is left as it is.
pub fn settle(out_dir: &Path, standing: &[(&str, Standing)], gates: &Gates) -> Result<(), Failure> {
    let mut standings = standings_in(out_dir, gates);
    let mut settled = false;
    for (gate, answer) in standing {
        if let Some(stood @ Standing::Pending) = standings.get_mut(*gate) {
            *stood = answer.clone();
            settled = true;
        }
    }
    if !settled {
        return Ok(());
    }
    write(out_dir, &crate::verdict::read(out_dir)?, &standings, gates)
}

#[cfg(test)]
#[path = "verifications_tests.rs"]
mod tests;
