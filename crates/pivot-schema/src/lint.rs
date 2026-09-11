//! `pivot-lint` (`PCAP2TEST_PIVOT_V3.md` §13): the semantic rules a JSON Schema
//! cannot state.
//!
//! The schema decides what a document may CONTAIN; lint decides whether what it
//! contains means anything. Two of its rules exist because the format is a
//! superset: a captured document must stay inside the generator subset, so no
//! pipeline can quietly start emitting constructs only a human can justify; and
//! an `alt`'s branches must be structurally discriminable, because the
//! interpreter commits on a branch's first message and never backtracks.
//!
//! Every diagnostic is copy for whoever has to fix the document: a stable rule
//! id, a location keyed by the human-facing id (`flow[id:s7]`), what is wrong,
//! and what to change. Lane names are deployment vocabulary, so unlike the
//! deployment linter this one is lane-agnostic: it reports what is true of the
//! document, and a driver decides what that costs a given lane.

mod accessors;
mod annotation_rules;
mod deviation_rules;
mod flow_rules;
mod must_fail_rules;
mod references;
mod routing_rules;
mod scoping_rules;
mod strings;
mod subset;
mod violation_rules;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::document::PivotV3;
use crate::flow::{FlowNode, Step};

/// How much a finding costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The document is wrong: a replay would do something other than what it
    /// reads as.
    Error,
    /// The document is suspect but runnable.
    Warning,
}

/// One finding.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    /// Stable rule id, `group/rule`.
    pub rule: &'static str,
    pub severity: Severity,
    /// Where, keyed by the human-facing id.
    pub path: String,
    /// What is wrong.
    pub message: String,
    /// What to change.
    pub hint: String,
}

/// Everything lint found in one document.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
}

impl Report {
    /// Whether anything blocks a replay.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }

    /// The rule ids present, order-independent — what a test asserts on.
    pub fn rules(&self) -> BTreeSet<&'static str> {
        self.diagnostics.iter().map(|d| d.rule).collect()
    }

    /// One block per diagnostic, for a terminal.
    pub fn render(&self) -> String {
        self.diagnostics
            .iter()
            .map(|d| {
                let severity = match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                };
                format!(
                    "{severity}: {} [{}]\n  {}\n  hint: {}\n",
                    d.path, d.rule, d.message, d.hint
                )
            })
            .collect()
    }

    pub(crate) fn push(
        &mut self,
        rule: &'static str,
        severity: Severity,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.diagnostics.push(Diagnostic {
            rule,
            severity,
            path: path.into(),
            message: message.into(),
            hint: hint.into(),
        });
    }

    pub(crate) fn error(
        &mut self,
        rule: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.push(rule, Severity::Error, path, message, hint);
    }

    pub(crate) fn warn(
        &mut self,
        rule: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.push(rule, Severity::Warning, path, message, hint);
    }
}

/// Run every rule over a document.
pub fn lint(pivot: &PivotV3) -> Report {
    let index = Index::of(pivot);
    let mut report = Report::default();
    references::check(&index, &mut report);
    routing_rules::check(&index, &mut report);
    flow_rules::check(&index, &mut report);
    deviation_rules::check(&index, &mut report);
    violation_rules::check(&index, &mut report);
    must_fail_rules::check(&index, &mut report);
    scoping_rules::check(&index, &mut report);
    accessors::check(&index, &mut report);
    annotation_rules::check(&index, &mut report);
    subset::check(&index, &mut report);
    report
}

/// Parse and lint a document from its text. A parse failure is itself one
/// diagnostic, so a caller has one shape to render either way.
pub fn lint_str(text: &str) -> Report {
    match PivotV3::from_json(text) {
        Ok(pivot) => {
            let mut report = Report::default();
            if !pivot.version_matches() {
                report.error(
                    "schema/version",
                    "pivot_version",
                    format!("document declares version {}", pivot.pivot_version),
                    format!("this linter models version {}", crate::PIVOT_VERSION),
                );
            }
            report.diagnostics.extend(lint(&pivot).diagnostics);
            report
        }
        Err(error) => {
            let mut report = Report::default();
            report.error(
                "schema/parse",
                "document",
                error.to_string(),
                "fix the document against `pivot-schema schema pivot`",
            );
            report
        }
    }
}

/// Where a step sits in the flow: which node, which branch of it, and where
/// inside that branch.
///
/// `branch` is what makes an `alt` safe to reference into. A step inside a
/// branch exists only on the run where that branch was chosen, so it may be
/// referenced ONLY from a later step of the SAME branch. From anywhere else the
/// reference is to the `alt` node's own id, which means "the alt completed,
/// whichever branch ran" — and the interpreter never has to define what a
/// reference onto a step that did not run would mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Place {
    /// Index in `flow`.
    pub node: usize,
    /// Which `alt` branch, where the step is inside one.
    pub branch: Option<usize>,
    /// Position within the branch. Every member of an `unordered` group shares
    /// one, because an order-free group has no internal order to reference
    /// across.
    pub inner: usize,
}

impl Place {
    /// The place a postcondition or a step-less deviation resolves at: after
    /// every node has run.
    pub const AFTER_EVERYTHING: Place = Place { node: usize::MAX, branch: None, inner: 0 };

    pub fn node(node: usize) -> Self {
        Place { node, branch: None, inner: 0 }
    }
}

/// Whether one place may reference another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The target ran earlier, on every run this one runs.
    Ok,
    /// The target has not run yet.
    Forward,
    /// The target is inside an `alt` branch this reference is not part of, so
    /// on some runs it never happens at all.
    CrossBranch,
}

/// Can a reference made AT `from` name something AT `target`?
pub(crate) fn reach(target: Place, from: Place) -> Reach {
    if target.node > from.node {
        return Reach::Forward;
    }
    if target.node < from.node {
        // Reaching back into a branch of an earlier `alt` is the cross-branch
        // case even from outside it: that step may never have run.
        return if target.branch.is_some() { Reach::CrossBranch } else { Reach::Ok };
    }
    if target.branch != from.branch {
        return Reach::CrossBranch;
    }
    if target.inner < from.inner {
        Reach::Ok
    } else {
        Reach::Forward
    }
}

/// Pre-computed lookups over one document.
pub(crate) struct Index<'a> {
    pub pivot: &'a PivotV3,
    pub endpoints: BTreeSet<&'a str>,
    pub actors: BTreeSet<&'a str>,
    pub legs: BTreeSet<&'a str>,
    /// The identity registry, by name.
    pub identities: BTreeMap<&'a str, &'a crate::identity::Identity>,
    /// Which call each leg belongs to — its caller leg, or one of its attempts'.
    /// A leg two calls both claim resolves to neither, so a join reference into
    /// it is refused rather than silently accepted.
    pub call_of_leg: BTreeMap<&'a str, Option<&'a str>>,
    /// Ids of `alt` nodes — the only ids `${step:<id>.branch}` may name.
    pub alts: BTreeSet<&'a str>,
    /// Every `early` id a step declares, to the legs that declare it. A leg owns
    /// its own fork tag space, so one id on two legs is two dialogs and
    /// `${early:<id>.…}` names neither.
    pub early: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// Every node id, including block ids, to its position in `flow`.
    pub nodes: BTreeMap<&'a str, usize>,
    /// Every message step by id, with where it sits.
    pub steps: BTreeMap<&'a str, (Place, &'a Step)>,
}

impl<'a> Index<'a> {
    pub fn of(pivot: &'a PivotV3) -> Self {
        let mut index = Index {
            pivot,
            endpoints: pivot.endpoints.iter().map(|e| e.id.as_str()).collect(),
            actors: pivot.actors.iter().map(|a| a.id.as_str()).collect(),
            legs: pivot.legs.iter().map(|l| l.id.as_str()).collect(),
            identities: pivot.identities.iter().map(|i| (i.name.as_str(), i)).collect(),
            call_of_leg: call_of_leg(pivot),
            alts: BTreeSet::new(),
            early: BTreeMap::new(),
            nodes: BTreeMap::new(),
            steps: BTreeMap::new(),
        };
        for (node, entry) in pivot.flow.iter().enumerate() {
            index.nodes.insert(entry.id(), node);
            match entry {
                FlowNode::Alt(alt) => {
                    index.alts.insert(alt.id.as_str());
                    for (branch, arm) in alt.branches.iter().enumerate() {
                        for (inner, step) in arm.steps.iter().enumerate() {
                            let place = Place { node, branch: Some(branch), inner };
                            index.steps.insert(step.id.as_str(), (place, step));
                        }
                    }
                }
                // An order-free group has no internal order, so its members all
                // sit at one place and cannot reference each other.
                FlowNode::Unordered(group) => {
                    for step in &group.steps {
                        index.steps.insert(step.id.as_str(), (Place::node(node), step));
                    }
                }
                FlowNode::Message(step) => {
                    index.steps.insert(step.id.as_str(), (Place::node(node), step));
                }
                FlowNode::Inject(_) => {}
            }
        }
        for (_, step) in index.steps.values() {
            if let Some(early) = step.early.as_deref() {
                index.early.entry(early).or_default().insert(step.leg.as_str());
            }
        }
        index
    }

    /// Where a step or block id sits, whichever kind of id it is.
    pub fn place_of(&self, id: &str) -> Option<Place> {
        self.steps
            .get(id)
            .map(|(place, _)| *place)
            .or_else(|| self.nodes.get(id).map(|node| Place::node(*node)))
    }

    /// Whether the document was generated from a capture, which is what the
    /// subset gate turns on.
    pub fn is_capture(&self) -> bool {
        self.pivot.case.origin == crate::case::Origin::Capture
    }

    /// Every message step in document order.
    pub fn all_steps(&self) -> impl Iterator<Item = (Place, &'a Step)> + '_ {
        let mut ordered: Vec<(Place, &'a Step)> = self.steps.values().copied().collect();
        ordered.sort_by_key(|(place, _)| (place.node, place.branch, place.inner));
        ordered.into_iter()
    }
}

/// Map every leg to the call that plays it. `None` where two calls claim one
/// leg, which no document should do and which lint reports where it matters.
fn call_of_leg(pivot: &PivotV3) -> BTreeMap<&str, Option<&str>> {
    let mut out: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for call in &pivot.calls {
        let legs = std::iter::once(call.caller_leg.as_str())
            .chain(call.attempts.iter().map(|a| a.leg.as_str()));
        for leg in legs {
            out.entry(leg)
                .and_modify(|owner| {
                    if *owner != Some(call.id.as_str()) {
                        *owner = None;
                    }
                })
                .or_insert(Some(call.id.as_str()));
        }
    }
    out
}

/// The location string every diagnostic keys on.
pub(crate) fn at(kind: &str, id: &str) -> String {
    format!("{kind}[id:{id}]")
}
