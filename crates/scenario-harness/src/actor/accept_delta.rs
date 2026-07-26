//! The actor-side accepted-delta consult (ADR-0024 §6): when a due reception
//! expectation meets a non-matching but classifiable inbound, the plan's
//! [`super::delta::AcceptedDeltaPolicy`] decides whether the substitution
//! satisfies the script. The policy VOCABULARY does not live here — see
//! [`super::delta`].

use std::collections::HashSet;

use tokio::time::Instant;

use super::delta::{
    AcceptedDelta, DeltaContext, DeltaDecision, DeltaReaction, DialogSnapshot, ExpectedStimulus,
    ObservedStimulus,
};
use super::goals::GoalStep;
use super::react::{cancel_pending_initial, react_in_dialog_request};
use super::runner::ActorState;
use super::script::{request_matches_kind, requeue_parked};
use super::state::{Observation, ResponseFact};
use crate::{ServerTxn, StepError};

/// The actor's dialog state offered to the accepted-delta policy: the count of
/// early dialogs open on the pending initial INVITE (UAS side: the tags this
/// leg emitted >100 provisionals under; caller side: the distinct fork tags
/// observed on >100 provisionals), plus the confirmed/phase facts.
pub(super) fn dialog_snapshot(st: &ActorState<'_>) -> DialogSnapshot {
    let phase = st.obs.with_snapshot(|s| s.leg(st.role).phase());
    let uas_initial_pending = st.pending_answer.is_some()
        || st.pending_prack_answer.is_some()
        || st.held_silent.is_some()
        || st.bound.as_ref().is_some_and(|t| {
            t.request().method.as_str() == "INVITE" && t.request().to.tag.is_none()
        })
        || st.parked.iter().any(|p| p.initial);
    let early_dialog_count = if uas_initial_pending {
        st.early_provisionals.len()
    } else if st.dialogs.pending_invite.is_some() {
        st.obs.with_snapshot(|s| {
            s.leg(st.role)
                .responses()
                .iter()
                .filter(|f| f.status > 100 && f.status < 200)
                .map(|f| f.early_tag.clone().unwrap_or_default())
                .collect::<HashSet<_>>()
                .len()
        })
    } else {
        0
    };
    DialogSnapshot { early_dialog_count, confirmed: st.dialogs.confirmed.is_some(), phase }
}

/// Bound an acceptance's `satisfies_steps` to `1..=remaining` (the due
/// expectation through the script tail) — a policy typo must fail fast with a
/// bounded error, never silently exhaust the script.
pub(super) fn check_satisfies_bound(
    st: &ActorState<'_>,
    rule: &'static str,
    satisfies_steps: usize,
) -> Result<(), StepError> {
    let remaining = st.goals.remaining_steps().count();
    if satisfies_steps == 0 || satisfies_steps > remaining {
        return Err(StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: format!(
                "accepted-delta rule {rule:?} must satisfy 1..={remaining} steps (the due \
                 expectation through the script tail), got {satisfies_steps}"
            ),
        });
    }
    Ok(())
}

/// The accepted-delta hook for an observed REQUEST (ADR-0024 §6): when this
/// actor's DUE goal is an `ExpectRequest` of a different kind, the plan's
/// policy decides whether the substitution satisfies the script. Acceptance
/// records the `AcceptedDelta` observation (never silent), advances the cursor
/// past the satisfied steps — so they neither fire nor trip the
/// consumed-target tombstone — performs the declared reaction, and re-scans
/// the parked queue. Returns `None` when the inbound was consumed here,
/// `Some(uas)` to continue the normal mismatch path (no policy, no due request
/// expectation, or `NotAccepted`).
pub(super) async fn try_accept_request_delta(
    st: &mut ActorState<'_>,
    uas: ServerTxn,
) -> Result<Option<ServerTxn>, StepError> {
    let Some(policy) = st.delta_policy.clone() else { return Ok(Some(uas)) };
    let Some(GoalStep::ExpectRequest { kind, .. }) = st.goals.next_step() else {
        return Ok(Some(uas));
    };
    let kind = *kind;
    if request_matches_kind(uas.request(), &kind) {
        // A matching inbound is never a delta (it parks for the script).
        return Ok(Some(uas));
    }
    let expected = ExpectedStimulus::Request(&kind);
    let observed = ObservedStimulus::Request(uas.request());
    let (expected_label, observed_label) = (expected.describe(), observed.describe());
    let decision = policy(&DeltaContext {
        role: st.role,
        expected,
        observed,
        dialog: dialog_snapshot(st),
    });
    let DeltaDecision::Accepted(AcceptedDelta { rule, satisfies_steps, reaction }) = decision
    else {
        return Ok(Some(uas));
    };
    check_satisfies_bound(st, rule, satisfies_steps)?;
    let now = Instant::now();
    st.obs.record(
        Observation::AcceptedDelta {
            leg: st.role,
            step: st.goals.position(),
            expected: expected_label,
            observed: observed_label,
            rule,
        },
        now,
    );
    for _ in 0..satisfies_steps {
        st.goals.advance();
    }
    match reaction {
        // The CANCEL-automatic mechanics riding the observed request: 200 its
        // hop, then 487 + terminate via the shared `cancel_pending_initial`
        // (no stray record — the `AcceptedDelta` entry is the record).
        DeltaReaction::TerminatePendingInitial => {
            let mut uas = uas;
            uas.respond(200, "OK").try_send().await?;
            cancel_pending_initial(st, now, false).await?;
        }
        // The reactive core's standard answer table for the method. An
        // observed CANCEL's standard handling IS the automatic (200 + 487) —
        // run it with the stray record suppressed, exactly like
        // `TerminatePendingInitial`: an accepted substitution never
        // double-books as divergence.
        DeltaReaction::Default => {
            if uas.request().method.as_str() == "CANCEL" {
                let mut uas = uas;
                uas.respond(200, "OK").try_send().await?;
                cancel_pending_initial(st, now, false).await?;
            } else {
                react_in_dialog_request(st, uas).await?;
            }
        }
    }
    // The cursor moved: a parked request only the satisfied steps could
    // consume must not starve.
    requeue_parked(st).await?;
    Ok(None)
}

/// The accepted-delta hook for an observed RESPONSE (ADR-0024 §6): consulted
/// by `ExpectResponse` on a status mismatch BEFORE failing `WrongStatus`.
/// Acceptance records the observation, consumes the response facts through the
/// observed one, and advances the cursor past the satisfied steps beyond the
/// due expectation (which the goal loop itself advances). A response
/// substitution's reaction must be [`DeltaReaction::Default`] — the reactive
/// follow-up (ACK / hop-ACK) already keyed on the observed status. Returns
/// `true` when accepted.
pub(super) fn try_accept_response_delta(
    st: &mut ActorState<'_>,
    expected_status: u16,
    fact: &ResponseFact,
    consumed_through: usize,
) -> Result<bool, StepError> {
    let Some(policy) = st.delta_policy.clone() else { return Ok(false) };
    let decision = policy(&DeltaContext {
        role: st.role,
        expected: ExpectedStimulus::Response { status: expected_status },
        observed: ObservedStimulus::Response { status: fact.status, reason: &fact.reason },
        dialog: dialog_snapshot(st),
    });
    let DeltaDecision::Accepted(AcceptedDelta { rule, satisfies_steps, reaction }) = decision
    else {
        return Ok(false);
    };
    check_satisfies_bound(st, rule, satisfies_steps)?;
    if !matches!(reaction, DeltaReaction::Default) {
        return Err(StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: format!(
                "accepted-delta rule {rule:?}: a response substitution takes DeltaReaction::Default"
            ),
        });
    }
    st.obs.record(
        Observation::AcceptedDelta {
            leg: st.role,
            step: st.goals.position(),
            expected: expected_status.to_string(),
            observed: fact.status.to_string(),
            rule,
        },
        Instant::now(),
    );
    st.resp_seen = consumed_through;
    // The due `ExpectResponse` is advanced by the goal loop; only the further
    // satisfied steps advance here.
    for _ in 1..satisfies_steps {
        st.goals.advance();
    }
    Ok(true)
}
