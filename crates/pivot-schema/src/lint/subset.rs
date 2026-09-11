//! The generator subset gate: what a CAPTURED document may contain.
//!
//! The format is one superset covering a replayed capture and a hand-written
//! test. That is only safe while the two stay distinguishable, and the way they
//! stay distinguishable is that a capture may carry nothing a capture cannot
//! justify. A packet trace shows what DID happen once; it never shows that an
//! absence was tolerable, that two orders were both acceptable, or that a value
//! should be read from a dialog at run time. A generator emitting `optional` or
//! a run-time accessor has inferred something, and inference belongs to a human
//! whose reasoning is reviewable — so the two exemptions below are the two the
//! generator puts ON THE PAGE. A tolerated caller-facing PROVISIONAL passes when
//! the document names the pass that derived it ([`SURPLUS_PROVISIONAL_FLAG`],
//! §6.9): the trace shows the platform emitting more provisionals toward the
//! caller than it took from the callee, and a surplus answering to no peer
//! emission is exactly the absence a relaying SUT is entitled to.
//! A `num:` composition is the one accessor a
//! capture DOES justify: it resolves statically through the identities and dial
//! forms the document itself declares, which is what makes a captured number
//! portable across lanes (§6.4).
//!
//! The gate runs the other way too: a captured document must carry what a
//! capture DOES justify — its provenance, its coordinates, its span — so a
//! document that lost them is caught here rather than at confrontation.

use std::collections::BTreeSet;

use crate::accessor::Accessor;
use crate::flow::FlowNode;
use crate::lint::strings::{deviation_strings, postcondition_strings, step_strings};
use crate::lint::{at, Index, Report};
use crate::postcondition::CdrExpectation;

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    if index.is_capture() {
        authored_constructs(index, report);
        capture_evidence(index, report);
    } else {
        for (_, step) in index.all_steps() {
            if step.observed.is_some() {
                report.warn(
                    "authored/observed-present",
                    at("flow", &step.id),
                    "an authored step carries a capture coordinate",
                    "`observed` pairs a step with a captured message; drop it, or state `origin: capture`",
                );
            }
        }
    }
}

fn authored_constructs(index: &Index<'_>, report: &mut Report) {
    let refuse = |report: &mut Report, rule: &'static str, path: String, what: &str| {
        report.error(
            rule,
            path,
            format!("a captured document carries {what}"),
            "a capture cannot justify it; author the case with `origin: authored`, or drop it",
        );
    };

    // Correlation cuts a case per call, so a capture yields one DOCUMENT per
    // call — several files per capture. `calls[]` plurality is the authored
    // construct that makes a call-limiter test expressible, and nothing a
    // packet trace shows asks for it.
    if index.pivot.calls.len() > 1 {
        report.error(
            "subset/several-calls",
            "calls",
            format!("a captured document plays {} calls", index.pivot.calls.len()),
            "a pcap yields one document per call; cut a case per call, or state `origin: authored`",
        );
    }
    // The ONE background shape a generated document states: the replaying SUT's
    // in-dialog OPTIONS audit, answered 200, asserting nothing. Anything else a
    // capture cannot justify — a count is an authored assertion, another method
    // or status an authored behaviour.
    for actor in &index.pivot.actors {
        for policy in &actor.background {
            let audit_shape = policy.r#match.method.eq_ignore_ascii_case("OPTIONS")
                && policy.respond.status == 200
                && policy.count.is_none();
            if !audit_shape {
                refuse(
                    report,
                    "subset/background",
                    at("actors", &actor.id),
                    "a background policy beyond the OPTIONS-audit shape (OPTIONS, 200, no count)",
                );
            }
        }
    }
    for node in &index.pivot.flow {
        let path = at("flow", node.id());
        match node {
            FlowNode::Alt(_) => refuse(report, "subset/alt", path.clone(), "an `alt`"),
            FlowNode::Unordered(_) => {
                refuse(report, "subset/unordered", path.clone(), "an `unordered` group")
            }
            FlowNode::Inject(_) => refuse(report, "subset/inject", path.clone(), "an `inject`"),
            FlowNode::Message(_) => {}
        }
        if !node.after().is_empty() {
            refuse(report, "subset/after", path, "an `after` ordering");
        }
    }
    // The ONE tolerated absence a capture justifies (§6.9, issue 106): a
    // caller-facing PROVISIONAL beyond the peer emissions that anchor it, on a
    // document that SAYS it derived one. The flag is what puts the inference in
    // front of a reviewer, which is the whole of what this gate asks (module
    // doc); the shape bound is what keeps the exemption from covering anything
    // else the generator might one day tolerate silently.
    let declared = index
        .pivot
        .case
        .annotations
        .as_ref()
        .is_some_and(|a| a.flags.iter().any(|f| f.kind == SURPLUS_PROVISIONAL_FLAG));
    for (_, step) in index.all_steps() {
        let path = at("flow", &step.id);
        if step.optional && !(declared && provisional_expect_shape(step)) {
            refuse(report, "subset/optional", path.clone(), "an optional expectation");
        }
        if !step.checks.is_empty() {
            refuse(report, "subset/checks", path.clone(), "inline field checks");
        }
        // Every string a step carries, from the one inventory the accessor
        // validator reads: a run-time accessor hidden in a frozen tier-2 ref is
        // exactly as unjustifiable as one in a header value. A `num:`
        // composition passes — the capture justifies it (module doc).
        for found in step_strings(step) {
            if Accessor::run_time_in(found.text) {
                refuse(
                    report,
                    "subset/accessor",
                    format!("{path}.{}", found.field),
                    "a run-time accessor",
                );
            }
        }
    }
    if let Some(postconditions) = &index.pivot.postconditions {
        let has_inline = match &postconditions.cdr {
            Some(CdrExpectation::Expected(cdr)) => !cdr.checks.is_empty(),
            _ => false,
        };
        if !postconditions.checks.is_empty() || has_inline {
            refuse(
                report,
                "subset/postcondition-checks",
                "postconditions".to_string(),
                "postcondition field checks",
            );
        }
        for found in postcondition_strings(postconditions) {
            if Accessor::run_time_in(found.text) {
                refuse(
                    report,
                    "subset/accessor",
                    format!("postconditions.{}", found.field),
                    "a run-time accessor",
                );
            }
        }
    }
    for deviation in &index.pivot.deviations {
        let path = at("deviations", &deviation.id);
        for found in deviation_strings(deviation) {
            if Accessor::run_time_in(found.text) {
                refuse(
                    report,
                    "subset/accessor",
                    format!("{path}.{}", found.field),
                    "a run-time accessor",
                );
            }
        }
        if matches!(deviation.value, Some(crate::deviation::CseqValue::Relative(_))) {
            refuse(report, "subset/accessor", path, "a CSeq relative to a run-time value");
        }
    }
}

/// The flag the surplus-provisional pass is obliged to write beside what it
/// stamped, so an `optional` in a captured document is never silent.
const SURPLUS_PROVISIONAL_FLAG: &str = "provisional-expect-surplus-tolerated";

/// The flag the relay-deficit pass is obliged to write beside what it derived,
/// so a second step on one capture coordinate is never silent (§6.9).
const DERIVED_PROVISIONAL_FLAG: &str = "relayed-provisional-expect-derived";

/// Whether a step has the shape both provisional exemptions are bounded to: an
/// `expect` of a provisional response. 100 is the stack's own and never
/// relayed, so it is outside the rules exactly as it is outside their passes.
fn provisional_expect_shape(step: &crate::flow::Step) -> bool {
    matches!(step.op, crate::flow::Op::Expect)
        && step.msg.status.is_some_and(|status| status > 100 && status < 200)
}

fn capture_evidence(index: &Index<'_>, report: &mut Report) {
    if index.pivot.case.source.is_none() {
        report.error(
            "capture/source-missing",
            "case.source",
            "a captured document does not say what it was cut from",
            "state `source.capture`, `source.call_groups` and `source.anonymized`",
        );
    }
    if index.pivot.timing.capture_span_ms.is_none() {
        report.error(
            "capture/span-missing",
            "timing.capture_span_ms",
            "a captured document states no capture span",
            "state the case's span: the last step's `observed.at_us` in milliseconds",
        );
    }
    // A capture coordinate pairs a step with the message it is compared against,
    // and the pairing is a lookup — so two steps may name one message where the
    // SUT emits it twice, which is what a DERIVED provisional expectation does.
    // Both halves gate, as they do for `optional`: the flag alone would exempt
    // every duplicate in the file, the shape alone a silent inference.
    let derived = index
        .pivot
        .case
        .annotations
        .as_ref()
        .is_some_and(|a| a.flags.iter().any(|f| f.kind == DERIVED_PROVISIONAL_FLAG));
    let mut seen: BTreeSet<(usize, usize)> = BTreeSet::new();
    for (_, step) in index.all_steps() {
        if let Some(observed) = &step.observed {
            if !seen.insert((observed.leg, observed.msg))
                && !(derived && provisional_expect_shape(step))
            {
                report.error(
                    "capture/observed-duplicated",
                    at("flow", &step.id),
                    "two captured steps name one captured message",
                    "give the step its own `observed`, or state the pass that derived it",
                );
            }
        }
        if step.observed.is_none() {
            report.error(
                "capture/observed-missing",
                at("flow", &step.id),
                "a captured step carries no capture coordinate",
                "state `observed`: the post-run confrontation pairs a run step with its captured message through it",
            );
        }
    }
}
