//! `rfc_violations`: every entry lands on a step of this document and names an
//! emitter this document declares.
//!
//! The anchor and the emitter are what make the list actionable — a reader
//! lands on the datagram, and a driver knows whether the entry gates. An entry
//! naming neither is a note, and notes live in `case.annotations`.

use crate::lint::{at, Index, Report};
use crate::violation::SUT_EMITTER;

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for violation in &index.pivot.rfc_violations {
        let path = at("rfc_violations", &violation.rule.to_string());
        if !index.steps.contains_key(violation.step.as_str()) {
            report.error(
                "ref/violation-step-unknown",
                &path,
                format!("`step` names {:?}, which is no step of this flow", violation.step),
                "anchor the violation on the flow step whose message breaks the rule",
            );
        }
        if violation.emitter != SUT_EMITTER && !index.actors.contains(violation.emitter.as_str()) {
            report.error(
                "ref/violation-emitter-unknown",
                &path,
                format!(
                    "`emitter` names {:?}, which is neither an actor nor `{SUT_EMITTER}`",
                    violation.emitter
                ),
                "name the actor that emits the violating message, or `sut` where the system under test does",
            );
        }
    }
}
