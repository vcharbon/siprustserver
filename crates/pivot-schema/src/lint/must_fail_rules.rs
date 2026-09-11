//! `must_fail` (§11.2): a declared failure lands on a step of this document and
//! on the SHAPE its own member predicts.
//!
//! A negative case passes only by failing exactly as declared, so a declaration
//! nothing can match would make the case unpassable, and one the document
//! itself contradicts would make it unfailable. Both are errors here, because
//! either turns the safeguard the declaration exists to be into a run that goes
//! green for the wrong reason.

use crate::flow::{Op, Step};
use crate::lint::{at, reach, Index, Place, Reach, Report};
use crate::must_fail::DeclaredFailure;

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    let all: Vec<(Place, &Step)> = index.all_steps().collect();
    let mut seen: Vec<(DeclaredFailure, &str)> = Vec::new();

    for declared in &index.pivot.must_fail {
        let path = at("must_fail", &declared.failure.to_string());
        let Some((place, step)) = index.steps.get(declared.step.as_str()).copied() else {
            report.error(
                "ref/must-fail-step-unknown",
                &path,
                format!("`step` names {:?}, which is no step of this flow", declared.step),
                "anchor the declaration on the flow step the failure happens at or immediately after",
            );
            continue;
        };
        if seen.contains(&(declared.failure, declared.step.as_str())) {
            report.error(
                "must-fail/duplicate",
                &path,
                format!("{} is declared twice on step {:?}", declared.failure, declared.step),
                "state one declaration per failure and anchor: a run produces the failure once",
            );
            continue;
        }
        seen.push((declared.failure, declared.step.as_str()));

        match declared.failure {
            DeclaredFailure::UnexpectedAck => unexpected_ack(&all, step, place, &path, report),
            DeclaredFailure::UnexpectedPrack => unexpected_prack(&all, step, place, &path, report),
            DeclaredFailure::UnexpectedCancel => {
                unexpected_cancel(&all, step, place, &path, report)
            }
        }
    }
}

/// The `unexpected-ack` shape: the anchor is the dialog-creating 2xx the
/// SCRIPTED PEER sends, and the flow states no ACK answering it.
///
/// Both halves are load-bearing. The peer has to be the SENDER, because the ACK
/// the failure is about is the PLATFORM's answer to it — where the peer merely
/// received the 2xx, the withheld ACK is the peer's own and nothing unexpected
/// arrives. And an ACK the flow already states is an ACK the run expects, so
/// the platform emitting one satisfies the document instead of failing it.
fn unexpected_ack(
    all: &[(Place, &Step)],
    step: &Step,
    place: Place,
    path: &str,
    report: &mut Report,
) {
    if step.op != Op::Send || !is_dialog_creating(step) {
        report.error(
            "must-fail/anchor-not-a-2xx-send",
            path,
            format!(
                "step {:?} is not a `send` of a 2xx to INVITE, so no platform ACK answers it",
                step.id
            ),
            "anchor `unexpected-ack` on the step where the scripted peer SENDS the dialog-creating 2xx the platform will ACK (§11.2)",
        );
        return;
    }
    if let Some(ack) = answering(all, step, place, "ACK") {
        report.error(
            "must-fail/anchor-already-acked",
            path,
            format!(
                "step {:?} is answered by ACK step {ack:?} on leg {:?}, so the platform's ACK is expected, not unexpected",
                step.id, step.leg
            ),
            "drop the declaration, or drop the ACK step the capture does not hold — a document cannot both expect an ACK and declare it unexpected",
        );
    }
}

/// The `unexpected-prack` shape: the anchor is the RELIABLE provisional the
/// SCRIPTED PEER sends, and the flow states no PRACK answering it.
///
/// The same two halves as `unexpected-ack`, one transaction earlier. The peer
/// has to be the SENDER, because the PRACK the failure is about is the
/// PLATFORM's answer (RFC 3262 §4) — where the peer merely received the
/// provisional, the withheld PRACK is the peer's own. And a PRACK the flow
/// already states is one the run expects, so the platform emitting it
/// satisfies the document instead of failing it.
fn unexpected_prack(
    all: &[(Place, &Step)],
    step: &Step,
    place: Place,
    path: &str,
    report: &mut Report,
) {
    if step.op != Op::Send || !is_reliable_provisional(step) {
        report.error(
            "must-fail/anchor-not-a-reliable-provisional-send",
            path,
            format!(
                "step {:?} is not a `send` of a reliable provisional to INVITE, so no platform PRACK answers it",
                step.id
            ),
            "anchor `unexpected-prack` on the step where the scripted peer SENDS the reliable provisional the platform will PRACK (§11.2)",
        );
        return;
    }
    if let Some(prack) = answering(all, step, place, "PRACK") {
        report.error(
            "must-fail/anchor-already-pracked",
            path,
            format!(
                "step {:?} is answered by PRACK step {prack:?} on leg {:?}, so the platform's PRACK is expected, not unexpected",
                step.id, step.leg
            ),
            "drop the declaration, or drop the PRACK step the capture does not hold — a document cannot both expect a PRACK and declare it unexpected",
        );
    }
}

/// The `unexpected-cancel` shape: the anchor is the FINAL response to INVITE
/// the SCRIPTED PEER sends, the flow states the source's own CANCEL on that leg
/// BEHIND it, and it states none AHEAD of it.
///
/// All three halves are load-bearing. The peer has to be the SENDER of the
/// final, because the transaction the CANCEL is late for is the PLATFORM's and
/// the anchor is what supplies its dialog. A CANCEL behind the anchor is the
/// whole claim — a document with none states no lateness, so there is nothing
/// this platform's prompt CANCEL could arrive ahead of. And a CANCEL AHEAD of
/// the anchor is one the flow expects in time, so the platform emitting one
/// satisfies the document instead of failing it.
fn unexpected_cancel(
    all: &[(Place, &Step)],
    step: &Step,
    place: Place,
    path: &str,
    report: &mut Report,
) {
    if step.op != Op::Send || !is_invite_final(step) {
        report.error(
            "must-fail/anchor-not-a-final-send",
            path,
            format!(
                "step {:?} is not a `send` of a final response to INVITE, so no transaction of \
                 this platform's completed on it",
                step.id
            ),
            "anchor `unexpected-cancel` on the step where the scripted peer SENDS the final the source's CANCEL arrived behind (§11.2)",
        );
        return;
    }
    if answering(all, step, place, "CANCEL").is_none() {
        report.error(
            "must-fail/anchor-has-no-late-cancel",
            path,
            format!(
                "no CANCEL step on leg {:?} sits behind step {:?}, so this document states no \
                 CANCEL the source sent late",
                step.leg, step.id
            ),
            "declare `unexpected-cancel` only where the capture holds the source's CANCEL AFTER the final it anchors on (§11.2)",
        );
        return;
    }
    if let Some(early) = preceding(all, step, place, "CANCEL") {
        report.error(
            "must-fail/anchor-already-cancelled",
            path,
            format!(
                "step {:?} is preceded by CANCEL step {early:?} on leg {:?}, so the platform's \
                 CANCEL is expected in time, not unexpected",
                step.id, step.leg
            ),
            "drop the declaration: a document that already expects the CANCEL ahead of the final has nothing to declare unexpected",
        );
    }
}

/// The id of the request the flow states answering this response: a `method`
/// step on the same leg that this step's own run reaches, which is the request
/// the platform's answer would satisfy.
fn answering<'a>(
    all: &[(Place, &'a Step)],
    step: &Step,
    place: Place,
    method: &str,
) -> Option<&'a str> {
    all.iter()
        .find(|(other_place, other)| {
            other.leg == step.leg
                && is_method(other, method)
                && reach(place, *other_place) == Reach::Ok
        })
        .map(|(_, other)| other.id.as_str())
}

/// The id of the request the flow states AHEAD of this response: a `method`
/// step on the same leg that reaches this step, so the run emits it first.
fn preceding<'a>(
    all: &[(Place, &'a Step)],
    step: &Step,
    place: Place,
    method: &str,
) -> Option<&'a str> {
    all.iter()
        .find(|(other_place, other)| {
            other.leg == step.leg
                && is_method(other, method)
                && reach(*other_place, place) == Reach::Ok
        })
        .map(|(_, other)| other.id.as_str())
}

/// Whether the step is a final response answering an INVITE.
fn is_invite_final(step: &Step) -> bool {
    step.msg.status.is_some_and(|s| s >= 200)
        && step.msg.cseq_method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
}

/// Whether the step is a 2xx answering an INVITE.
fn is_dialog_creating(step: &Step) -> bool {
    step.msg.status.is_some_and(|s| (200..300).contains(&s))
        && step.msg.cseq_method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
}

/// Whether the step is a provisional to INVITE that states reliability: a
/// `Require: 100rel` or an `RSeq` among its frozen headers, or an `RSeq`
/// existence check (an expect states the stack-owned value that way). Read off
/// the document's own header list — lint inspects the schema, never the wire.
fn is_reliable_provisional(step: &Step) -> bool {
    let provisional = step.msg.status.is_some_and(|s| (101..200).contains(&s))
        && step.msg.cseq_method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"));
    provisional
        && (step.msg.headers.iter().any(|h| {
            (h.name.eq_ignore_ascii_case("Require")
                && h.value.to_ascii_lowercase().contains("100rel"))
                || h.name.eq_ignore_ascii_case("RSeq")
        }) || step.msg.headers_present.iter().any(|n| n.eq_ignore_ascii_case("RSeq")))
}

/// Whether the step is a request of `method`.
fn is_method(step: &Step, method: &str) -> bool {
    step.msg.status.is_none()
        && step.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(method))
}
