//! `rfc_violations`: every entry lands on a step of this document and names an
//! emitter this document declares.
//!
//! The anchor and the emitter are what make the list actionable — a reader
//! lands on the datagram, and a driver knows whether the entry gates. An entry
//! naming neither is a note, and notes live in `case.annotations`. A `relays`
//! names the scripted party's entry a `sut` entry carries on: it rides a `sut`
//! entry of a relayable rule and lands on that party's entry of the same rule,
//! earlier and on another leg of the same call.

use crate::lint::{at, Index, Report};
use crate::violation::{RfcRule, SUT_EMITTER};

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
        let Some(origin) = &violation.relays else { continue };
        if let Some((code, detail)) = relays_defect(index, violation, origin) {
            report.error(
                code,
                &path,
                detail,
                "a `relays` rides a `sut` entry of a relayable rule and names an earlier step, \
                 on another leg of the same call, carrying a scripted party's entry of that rule",
            );
        }
    }
}

/// What is wrong with `violation`'s `relays` naming `origin`, by lint code;
/// `None` where it carries on a scripted party's entry as §11.1 says.
fn relays_defect(
    index: &Index<'_>,
    violation: &crate::violation::RfcViolation,
    origin: &str,
) -> Option<(&'static str, String)> {
    let rule = violation.rule;
    if violation.emitter != SUT_EMITTER {
        let detail =
            format!("`relays` rides {:?}'s entry; only a `sut` entry relays", violation.emitter);
        return Some(("ref/violation-relays-emitter", detail));
    }
    if !RfcRule::RELAYABLE.contains(&rule) {
        let detail = format!("{rule} is not a rule a forwarded message carries on");
        return Some(("ref/violation-relays-rule", detail));
    }
    let stated = index
        .pivot
        .rfc_violations
        .iter()
        .any(|v| v.rule == rule && v.step == origin && v.emitter != SUT_EMITTER);
    if !stated {
        let detail =
            format!("`relays` names {origin:?}, which carries no scripted party's {rule} entry");
        return Some(("ref/violation-relays-unstated", detail));
    }
    let steps = index.pivot.steps();
    let at = |id: &str| steps.iter().position(|s| s.id == id);
    let (Some(from), Some(to)) = (at(origin), at(&violation.step)) else { return None };
    let call = |leg: &str| index.call_of_leg.get(leg).copied().flatten();
    let (party_leg, sut_leg) = (&steps[from].leg, &steps[to].leg);
    if from >= to || party_leg == sut_leg || call(party_leg) != call(sut_leg) {
        let detail = format!(
            "`relays` names {origin:?} on leg {party_leg:?}; the party's entry must come earlier, \
             on another leg of the same call than {:?} on leg {sut_leg:?}",
            violation.step
        );
        return Some(("ref/violation-relays-order", detail));
    }
    None
}
