//! The `calls` block: one well-formed attempt chain per call, and a lane
//! verdict that matches what the document actually asks a lane to do.
//!
//! The claim rule is the one worth stating twice. A lane numbers a called
//! position by its ATTEMPT INDEX, so every branch's attempt `s` is dialed one
//! number; two `ruri-pos` claims that land on one endpoint with the same index
//! are indistinguishable at replay, and a document that says a lane can run it
//! anyway is claiming something the lane cannot deliver.

use std::collections::{BTreeMap, BTreeSet};

use crate::call::Cause;
use crate::flow::Step;
use crate::lint::{Index, Reach, Report, at, reach};
use crate::placement::ClaimBy;

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    chains(index, report);
    lanes(index, report);
}

fn chains(index: &Index<'_>, report: &mut Report) {
    for call in &index.pivot.calls {
        let path = at("calls", &call.id);
        if !index.legs.contains(call.caller_leg.as_str()) {
            report.error(
                "ref/call-caller-leg-unknown",
                &path,
                format!("`caller_leg` names {:?}, which is not declared", call.caller_leg),
                "name a leg from `legs`",
            );
        }
        refused(index, report, call, &path);
        abandoned(index, report, call, &path);
        if call.attempts.is_empty() && call.refused.is_none() && call.abandoned.is_none() {
            report.error(
                "call/no-attempts",
                &path,
                "the call dials nobody",
                "a call states at least one attempt, the `refused` decision that dialed none, or the `abandoned` the caller left it on",
            );
        }

        let mut keys = BTreeSet::new();
        let mut depth: BTreeMap<u32, u32> = BTreeMap::new();
        for attempt in &call.attempts {
            let at_attempt = format!("{path}.attempts[{}][{}]", attempt.branch, attempt.position);
            if !keys.insert((attempt.branch, attempt.position)) {
                report.error(
                    "attempt/position-duplicate",
                    &at_attempt,
                    "two attempts share one (branch, position)",
                    "`(branch, position)` is the chain's stable key; renumber one of them",
                );
            }
            depth
                .entry(attempt.branch)
                .and_modify(|high| *high = (*high).max(attempt.position))
                .or_insert(attempt.position);
            if !index.legs.contains(attempt.leg.as_str()) {
                report.error(
                    "ref/attempt-leg-unknown",
                    &at_attempt,
                    format!("`leg` names {:?}, which is not declared", attempt.leg),
                    "name a leg from `legs`",
                );
            }
            if !index.identities.contains_key(attempt.callee.identity.as_str()) {
                report.error(
                    "ref/callee-identity-unknown",
                    &at_attempt,
                    format!(
                        "`callee.identity` names {:?}, which is not in the registry",
                        attempt.callee.identity
                    ),
                    "name an entry from `identities`",
                );
            }
            joined(index, report, call, attempt, &at_attempt);
            cited_cause(index, report, attempt, &at_attempt);
            if !attempt.no_answer_ms_is_declarable() {
                report.error(
                    "attempt/no-answer-undeclarable",
                    &at_attempt,
                    "`no_answer_ms` is stated without its `no-answer` cause, missing under it, or outside the armable band",
                    "state the dwell exactly when the cause is `no-answer`, between 1000 and 3600000 ms",
                );
            }
        }
        for attempt in &call.attempts {
            let last = depth.get(&attempt.branch).copied().unwrap_or(attempt.position);
            if attempt.position < last && attempt.cause.is_none() {
                report.error(
                    "attempt/cause-missing",
                    format!("{path}.attempts[{}][{}]", attempt.branch, attempt.position),
                    "the platform moved on from this attempt without the document saying why",
                    "state the `cause` the capture shows; a chained attempt always has one",
                );
            }
        }
    }
}

/// A refused call dials nobody and points at the final the caller got.
///
/// The step must be a non-2xx final on the CALLER leg: it is what the routing
/// layer is compiled to answer with, so a step naming anything else would arm a
/// lane with a status the capture never showed the caller.
fn refused(index: &Index<'_>, report: &mut Report, call: &crate::call::Call, path: &str) {
    let Some(refused) = &call.refused else { return };
    if !call.attempts.is_empty() {
        report.error(
            "call/refused-with-attempts",
            path,
            "the call is stated refused and still dials",
            "a refusal dials nobody; drop `refused` or drop the chain",
        );
    }
    let Some((_, step)) = index.steps.get(refused.step.as_str()).copied() else {
        report.error(
            "ref/refused-step-unknown",
            path,
            format!("`refused.step` names {:?}, which is no step", refused.step),
            "name the flow step carrying the final the caller was answered with",
        );
        return;
    };
    let answered = step.leg == call.caller_leg && matches!(step.msg.status, Some(300..=699));
    if !answered {
        report.error(
            "call/refused-step-not-a-final",
            path,
            format!(
                "`refused.step` names {:?}, which is not a 3xx-6xx final on caller leg {:?}",
                refused.step, call.caller_leg
            ),
            "name the caller-leg step carrying the final the refusal answered with",
        );
    }
}

/// An abandoned call dials nobody and points at the CANCEL the caller sent.
///
/// The step must be a CANCEL the CALLER LEG emitted: an abandon is an act of
/// the caller, and a step naming anything else — the platform's `200`, a CANCEL
/// on some other leg — would state the caller left on evidence the caller never
/// produced. It is exclusive with a refusal, since one vantage cannot show both
/// the platform deciding and the caller leaving before it did.
fn abandoned(index: &Index<'_>, report: &mut Report, call: &crate::call::Call, path: &str) {
    let Some(abandoned) = &call.abandoned else { return };
    if !call.attempts.is_empty() {
        report.error(
            "call/abandoned-with-attempts",
            path,
            "the call is stated abandoned before any dial and still dials",
            "an abandoned call dials nobody; drop `abandoned` or drop the chain",
        );
    }
    if call.refused.is_some() {
        report.error(
            "call/abandoned-with-refusal",
            path,
            "the call is stated both refused and abandoned",
            "state the one the capture shows: the platform's own final, or the caller's CANCEL",
        );
    }
    let Some((_, step)) = index.steps.get(abandoned.step.as_str()).copied() else {
        report.error(
            "ref/abandoned-step-unknown",
            path,
            format!("`abandoned.step` names {:?}, which is no step", abandoned.step),
            "name the flow step carrying the CANCEL the caller sent",
        );
        return;
    };
    let cancelled = step.leg == call.caller_leg
        && step.op == crate::flow::Op::Send
        && step.msg.method.as_deref() == Some("CANCEL");
    if !cancelled {
        report.error(
            "call/abandoned-step-not-a-cancel",
            path,
            format!(
                "`abandoned.step` names {:?}, which is not a CANCEL sent on caller leg {:?}",
                abandoned.step, call.caller_leg
            ),
            "name the caller-leg step carrying the CANCEL the caller abandoned the call with",
        );
    }
}

/// A `cause` cites the attempt's own DIALOG-CREATING final or an actual closer,
/// and nothing else.
///
/// An in-dialog negative answer does not close a leg: the dialog it answers is
/// already established, and the platform that then releases the leg does so with
/// a BYE. A cause citing a status the leg only ever saw in-dialog attributes the
/// release to a message that never released anything — which reads, on the
/// attempt, as "this callee answered that", when the callee answered 200.
fn cited_cause(
    index: &Index<'_>,
    report: &mut Report,
    attempt: &crate::call::Attempt,
    path: &str,
) {
    let Some(cause) = attempt.cause else { return };
    let cited = match cause {
        Cause::External(status) | Cause::Redirect(status) => Some(status),
        _ => None,
    };
    if let Some(status) = cited {
        let finals: Vec<&Step> = index
            .all_steps()
            .filter(|(_, step)| step.leg == attempt.leg && step.msg.status == Some(status))
            .map(|(_, step)| step)
            .collect();
        if !finals.is_empty() && finals.iter().all(|step| step.in_dialog) {
            report.error(
                "cause/in-dialog-final",
                path,
                format!(
                    "`cause` cites a {status}, and every {status} on leg {:?} is in-dialog",
                    attempt.leg
                ),
                "an in-dialog final is `cause_evidence`; state the closer that ended the attempt (`closed:bye`) as the cause",
            );
        }
    }
    if cause == Cause::ClosedBye
        && !index
            .all_steps()
            .any(|(_, step)| step.leg == attempt.leg && step.msg.method.as_deref() == Some("BYE"))
    {
        report.error(
            "cause/closer-missing",
            path,
            format!("`cause` is `closed:bye` and leg {:?} carries no BYE", attempt.leg),
            "cite the closer the flow shows, or state the cause the attempt's own final justifies",
        );
    }
}

/// A joined leg states ONE reason it is in the chain, and points at a join
/// event of its own call.
fn joined(
    index: &Index<'_>,
    report: &mut Report,
    call: &crate::call::Call,
    attempt: &crate::call::Attempt,
    path: &str,
) {
    let Some(joined) = &attempt.joined_by else { return };
    let Some((place, step)) = index.steps.get(joined.step.as_str()).copied() else {
        report.error(
            "ref/joined-step-unknown",
            path,
            format!("`joined_by.step` names {:?}, which is no step", joined.step),
            "name the flow step that joined this leg to the call",
        );
        return;
    };
    if index.call_of_leg.get(step.leg.as_str()).copied().flatten() != Some(call.id.as_str()) {
        report.error(
            "attempt/joined-step-other-call",
            path,
            format!(
                "`joined_by.step` names {:?}, which runs on leg {:?} — not a leg of call {:?}",
                joined.step, step.leg, call.id
            ),
            "a join happens inside one call; name a step of a leg this call plays",
        );
    }
    // The chain is not conditional: an attempt entry either belongs to the call
    // or does not, and a step inside an `alt` branch runs only on the run that
    // chose it. The alt's own id is no substitute here — a join names the
    // message that performed it, not the block that may have contained it.
    if place.branch.is_some() {
        report.error(
            "attempt/joined-step-conditional",
            path,
            format!("`joined_by.step` names {:?}, a step inside an `alt` branch", joined.step),
            "a joined leg is stated unconditionally; name a step that runs on every run, or model the two outcomes as separate documents",
        );
    }
    // The join is what ADDED the leg, so it precedes everything the leg does.
    let first = index
        .all_steps()
        .find(|(_, other)| other.leg == attempt.leg)
        .map(|(place, _)| place);
    if let Some(first) = first {
        if reach(place, first) != Reach::Ok {
            report.error(
                "attempt/joined-step-late",
                path,
                format!(
                    "`joined_by.step` names {:?}, which does not run before the first message on leg {:?}",
                    joined.step, attempt.leg
                ),
                "the join is what added the leg; name the step that precedes the leg's own first message",
            );
        }
    }
}

fn lanes(index: &Index<'_>, report: &mut Report) {
    let pivot = index.pivot;
    if pivot.case.lanes.is_empty() {
        report.error(
            "lanes/unstated",
            "case.lanes",
            "the document states no lane verdict",
            "state per lane whether it can replay this case, `ok` or `blocked:<reason>`",
        );
    }

    // A lane dials attempt `s` of every branch one number, so two `ruri-pos`
    // claims sharing (index, endpoint) are one number at replay.
    let endpoint_of_leg: BTreeMap<&str, &str> = pivot
        .legs
        .iter()
        .filter_map(|leg| {
            let actor = pivot.actors.iter().find(|a| a.id == leg.actor)?;
            Some((leg.id.as_str(), actor.endpoint.as_str()))
        })
        .collect();
    let claims_by_ruri: BTreeSet<&str> = pivot
        .actors
        .iter()
        .filter(|a| a.claim.map(|c| c.by == ClaimBy::RuriPos).unwrap_or(false))
        .map(|a| a.id.as_str())
        .collect();
    let mut seen: BTreeSet<(u32, &str)> = BTreeSet::new();
    let mut ambiguous = false;
    for call in &pivot.calls {
        for attempt in &call.attempts {
            let Some(leg) = pivot.legs.iter().find(|l| l.id == attempt.leg) else { continue };
            if !claims_by_ruri.contains(leg.actor.as_str()) {
                continue;
            }
            let endpoint = endpoint_of_leg.get(attempt.leg.as_str()).copied().unwrap_or("");
            if !seen.insert((attempt.position, endpoint)) {
                ambiguous = true;
            }
        }
    }
    if ambiguous {
        for (lane, verdict) in pivot.case.lanes.iter().filter(|(_, v)| v.is_ok()) {
            report.error(
                "claim/same-number-ambiguous",
                format!("case.lanes.{lane}"),
                format!(
                    "the lane claims {verdict}, but two `ruri-pos` claims share an attempt index on one endpoint"
                ),
                "a lane dials attempt `s` of every branch one number; block the lane with a reason",
            );
        }
    }
}
