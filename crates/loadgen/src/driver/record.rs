//! What the driver hands the reporter beside a call's verdict: the record of
//! a call that never ran, and a sample's phase-annotated detail.

use std::sync::Arc;
use std::time::Duration;

use crate::class::{CallOutcome, ResultClass};
use crate::ctx::CallCtx;
use crate::outcome::OutcomeSink;
use crate::report::{CallEvidence, RenderedSample, Reporter};
use crate::scenarios::ScenarioId;

/// Record a call that never ran (refused before any datagram) and close its
/// in-flight count.
pub(super) fn record_unrun(
    reporter: &Reporter,
    outcomes: &Option<Arc<OutcomeSink>>,
    id: ScenarioId,
    outcome: CallOutcome,
) {
    if let Some(sink) = outcomes {
        match &outcome {
            CallOutcome::Rejected(reason) => sink.not_placed(id, reason),
            _ => sink.generator_failure(id, "bind_refused"),
        }
    }
    let class = ResultClass::from(&outcome);
    let case = outcome.case(None);
    let sample = reporter.wants_sample(id, &class, &case).then(|| RenderedSample {
        html: None,
        detail: outcome.detail(),
        e2e_ms: 0.0,
    });
    reporter.record(
        id,
        &outcome,
        &case,
        Duration::ZERO,
        &[],
        CallEvidence { sample, ..CallEvidence::default() },
    );
    reporter.dec_inflight();
}

/// Append the call's lifecycle phase trail (`connected@1234ms`, `reinvited@…`) to
/// a NOK sample's one-line detail, so a sampled failing flow says WHICH phase it
/// reached before it failed.
/// `None`/empty detail (an OK call) is left untouched.
pub(super) fn phase_annotated_detail(detail: Option<String>, ctx: &CallCtx) -> Option<String> {
    let mut detail = detail?;
    // Scenario-attached diagnostic notes (e.g. the actor settle barrier's
    // still-open obligations) — detail-channel only, never a report key.
    let notes = ctx.notes();
    if !notes.is_empty() {
        detail = format!("{detail} [{}]", notes.join("; "));
    }
    let phases = ctx.phases();
    if phases.is_empty() {
        return Some(detail);
    }
    let start = ctx.start_instant();
    let trail = phases
        .iter()
        .map(|(name, at)| format!("{name}@{}ms", at.saturating_duration_since(start).as_millis()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{detail} [phases: {trail}]"))
}
