//! The [`super::endpoint::Disposition::Scripted`] machinery: the parked-request
//! queue predicates, requeue-on-advance, and the scripted reception / respond
//! goals (`ExpectRequest` / `ExpectResponse` / `Respond` / `RespondTemplate`).
//! The reactive (non-scripted) answer table does NOT live here — see
//! [`super::react`].

use tokio::time::Instant;

use super::accept_delta::try_accept_response_delta;
use super::answer::{
    answer_initial_invite, arm_reject_final, discharge_on_teardown, note_uas_answered,
    provisional_reason, reject_reason,
};
use super::endpoint::SUBFLOW_REALIGN;
use super::goals::{BodyExpect, EarlyId, GoalStep, RequestKind};
use super::ledger::{ObligationKey, ObligationKind};
use super::observe::{observe_reception, ReceivedMessage};
use super::react::{cancel_pending_initial, react_in_dialog_request};
use super::runner::{ActorState, ParkedRequest};
use super::select::{body_facts, select_parked};
use super::state::{Observation, ResponseFact, SubflowState};
use crate::StepError;
use sip_message::{EmitOpts, MatchOpts, MessageTemplate, SipMessage, SipRequest};

/// Whether the goal arm may fire the NEXT pending goal: a reception (or
/// binding-consuming) goal additionally requires its consumable — a new
/// response fact, a matching parked request, or a bound/parked transaction
/// (a tombstoned binding enables the arm so it FAILS fast, never by timeout).
pub(super) fn goal_arm_enabled(st: &ActorState<'_>) -> bool {
    match st.goals.next_step() {
        Some(GoalStep::ExpectRequest { kind, .. }) => {
            st.parked.iter().any(|p| parked_matches(p, kind))
                || (matches!(kind, RequestKind::Initial) && st.parked_initial_consumed.is_some())
        }
        Some(GoalStep::RespondTemplate { .. } | GoalStep::Respond { .. }) => {
            st.bound.is_some()
                || st.parked.iter().any(|p| p.initial)
                || st.parked_initial_consumed.is_some()
        }
        Some(GoalStep::ExpectResponse { status, cseq_method, .. }) => {
            let need_final = *status >= 200;
            let pin = cseq_method.as_deref();
            st.obs
                .with_snapshot(|s| s.leg_response_ready(st.role, st.resp_seen, need_final, pin))
        }
        Some(
            GoalStep::ObserveFinal { cseq_method, .. }
            | GoalStep::ExpectFinal { cseq_method, .. },
        ) => {
            let pin = cseq_method.as_deref();
            st.obs.with_snapshot(|s| s.leg_response_ready(st.role, st.resp_seen, true, pin))
        }
        _ => true,
    }
}

/// Whether a parked request satisfies an `ExpectRequest`'s kind.
pub(super) fn parked_matches(p: &ParkedRequest, kind: &RequestKind) -> bool {
    match kind {
        RequestKind::Initial => p.initial,
        RequestKind::InDialog(m) => !p.initial && p.txn.request().method().as_str() == m.as_str(),
        RequestKind::Cancel => !p.initial && p.txn.request().method().as_str() == "CANCEL",
    }
}

/// Whether a remaining scripted goal will consume/answer the parked initial
/// INVITE. Walks the remaining goals tracking the binding a `RespondTemplate`/
/// `Respond` would take at that point: an `ExpectRequest{Initial}` always
/// parks; a respond parks only when NO in-dialog `ExpectRequest` binding is
/// pending before it (else it answers that bound request, not the initial) —
/// so a stray initial never over-parks behind an in-dialog script tail.
pub(super) fn scripted_wants_initial(st: &ActorState<'_>) -> bool {
    let mut bound_pending = st.bound.is_some();
    for s in st.goals.remaining_steps() {
        match s {
            GoalStep::ExpectRequest { kind: RequestKind::Initial, .. } => return true,
            GoalStep::ExpectRequest { kind: RequestKind::InDialog(_), .. } => {
                bound_pending = true;
            }
            // A Cancel expectation consumes the parked CANCEL, never a binding.
            GoalStep::ExpectRequest { kind: RequestKind::Cancel, .. } => {}
            GoalStep::RespondTemplate { template, .. } => {
                if !bound_pending {
                    return true;
                }
                // A final consumes the pending binding; a provisional keeps it.
                if template.status().is_some_and(|(s, _)| s >= 200) {
                    bound_pending = false;
                }
            }
            GoalStep::Respond { status } => {
                if !bound_pending {
                    return true;
                }
                if *status >= 200 {
                    bound_pending = false;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a remaining `ExpectRequest` will consume an in-dialog request of
/// this method.
pub(super) fn scripted_wants_in_dialog(st: &ActorState<'_>, method: &str) -> bool {
    st.goals.remaining_steps().any(|s| {
        matches!(s, GoalStep::ExpectRequest { kind: RequestKind::InDialog(m), .. }
            if m.as_str() == method)
    })
}

/// Whether a remaining `ExpectRequest{Cancel}` claims the inbound CANCEL —
/// the precedence gate: a scripted claim always parks the CANCEL; only
/// without one does the automatic (200 + 487) fire.
pub(super) fn scripted_wants_cancel(st: &ActorState<'_>) -> bool {
    st.goals
        .remaining_steps()
        .any(|s| matches!(s, GoalStep::ExpectRequest { kind: RequestKind::Cancel, .. }))
}

/// Requeue-on-advance: auto-react every parked request no remaining goal can
/// consume (recorded as a serviced stray) — a parked request never starves
/// behind a script that moved past it.
pub(super) async fn requeue_parked(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let mut i = 0;
    while i < st.parked.len() {
        let keep = if st.parked[i].initial {
            scripted_wants_initial(st)
        } else if st.parked[i].txn.request().method().as_str() == "CANCEL" {
            scripted_wants_cancel(st)
        } else {
            let method = st.parked[i].txn.request().method().as_str().to_string();
            scripted_wants_in_dialog(st, &method)
        };
        if keep {
            i += 1;
            continue;
        }
        let entry = st.parked.remove(i);
        let method = entry.txn.request().method().as_str().to_string();
        st.obs.record(
            Observation::ServicedStray {
                leg: st.role,
                method: method.clone(),
                action: "auto-reacted on advance",
            },
            now,
        );
        if entry.initial {
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            answer_initial_invite(st, entry.txn, None).await?;
        } else if method == "CANCEL" {
            // Its 200 went out at park time — only the automatic's 487 half
            // remains for the still-pending INVITE target.
            cancel_pending_initial(st, now, true).await?;
        } else {
            react_in_dialog_request(st, entry.txn).await?;
        }
    }
    Ok(())
}

/// Whether an inbound request satisfies an `ExpectRequest`'s kind — the raw
/// twin of [`parked_matches`], evaluated before parking.
pub(super) fn request_matches_kind(req: &SipRequest, kind: &RequestKind) -> bool {
    let initial = req.method().as_str() == "INVITE" && req.to().tag().is_none();
    match kind {
        RequestKind::Initial => initial,
        RequestKind::InDialog(m) => !initial && req.method().as_str() == m.as_str(),
        RequestKind::Cancel => !initial && req.method().as_str() == "CANCEL",
    }
}

/// Answer the BOUND server transaction — the shared realization of
/// `RespondTemplate` (template payload) and `Respond` (policy payload). The
/// binding is the nearest preceding `ExpectRequest`'s consumed transaction,
/// else the parked initial INVITE. A status < 200 responds WITHOUT consuming
/// the binding; >= 200 consumes it with the disposition-equivalent bookkeeping
/// (dialog confirm / reject hop-ACK / teardown), keyed on the bound request.
pub(super) async fn drive_respond(
    st: &mut ActorState<'_>,
    status: u16,
    template: Option<(&MessageTemplate, EmitOpts)>,
    early: Option<EarlyId>,
    step_name: &'static str,
) -> Result<(), StepError> {
    let now = Instant::now();
    let use_bound = st.bound.is_some();
    let parked_initial = st.parked.iter().position(|p| p.initial);
    if !use_bound && parked_initial.is_none() {
        // Fail-fast, bounded: the target was consumed by an automatic
        // (CANCEL → 487) or never existed — never a goal timeout.
        let detail = match st.parked_initial_consumed {
            Some(consumed) => format!(
                "{step_name}: the bound initial INVITE was consumed by an automatic ({consumed})"
            ),
            None => format!("{step_name}: no bound or parked transaction to answer"),
        };
        return Err(StepError::UnexpectedKind { who: st.role.to_string(), detail });
    }

    if status < 200 {
        // Provisional: respond in place, binding NOT consumed
        // (provisional-then-final on ONE server transaction, §17.2.1).
        let txn = match st.bound.as_mut() {
            Some(t) => t,
            None => &mut st.parked[parked_initial.expect("checked above")].txn,
        };
        // A >100 provisional on the initial INVITE opens (or re-rides) an early
        // dialog — tracked per tag for `DialogSnapshot::early_dialog_count`.
        if status > 100
            && txn.request().method().as_str() == "INVITE"
            && txn.request().to().tag().is_none()
        {
            st.early_provisionals.insert(early.unwrap_or("").to_string());
        }
        let mut r = match template {
            Some((tmpl, opts)) => txn.respond_template(tmpl, opts),
            None => txn.respond(status, provisional_reason(status)),
        };
        if let Some(id) = early {
            // The fork id IS the fork's To-tag (RFC 3261 §12.1.2) — distinct
            // early dialogs on the one transaction.
            r = r.with_to_tag(id);
        }
        r.try_send().await?;
        st.obs.record(Observation::LegEarly { leg: st.role }, now);
        return Ok(());
    }

    // Final: consume the binding.
    let mut txn = match st.bound.take() {
        Some(t) => t,
        None => st.parked.remove(parked_initial.expect("checked above")).txn,
    };
    if let Some(id) = early {
        // The final's fork id names the WINNER: its tag becomes the sticky
        // dialog tag; the losing forks simply never receive a final (the
        // existing forked-UAS surface settles them).
        txn.adopt_to_tag(id);
    }
    let req_method = txn.request().method().as_str().to_string();
    let is_initial = req_method == "INVITE" && txn.request().to().tag().is_none();
    let cseq = txn.request().cseq().seq();

    {
        // `respond_template` derives status from the template; an early winner
        // tag was adopted above, so no per-response tag is needed here.
        let r = match template {
            Some((tmpl, opts)) => txn.respond_template(tmpl, opts),
            None if (200..300).contains(&status)
                && (req_method == "INVITE" || req_method == "UPDATE") =>
            {
                // A policy 2xx to an offer is never bodyless (RFC 3264 §5).
                txn.respond(status, "OK").with_sdp(st.media.answer_sdp().unwrap_or(crate::ANSWER_SDP))
            }
            None if (200..300).contains(&status) => txn.respond(status, "OK"),
            None => txn.respond(status, reject_reason(status)),
        };
        r.try_send().await?;
    }

    if (200..300).contains(&status) {
        if is_initial {
            note_uas_answered(st, &txn);
            st.feed.on_answer_sent.stamp(st.ctx);
        } else if req_method == "INVITE" {
            // A scripted 200 to a re-INVITE — same realign bookkeeping as the
            // reactive answer (the ACK confirms the sub-flow).
            st.answered_reinvites.insert(cseq);
            st.obs.record(
                Observation::RequestSent {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                    detail: "realign 200 awaiting ACK".to_string(),
                },
                now,
            );
            st.obs.record(
                Observation::Subflow {
                    leg: st.role,
                    name: SUBFLOW_REALIGN,
                    to: SubflowState::Answered,
                },
                now,
            );
        } else if req_method == "BYE" {
            // A scripted 200 to a BYE tears this leg down (§15).
            discharge_on_teardown(st, now);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
    } else if req_method == "INVITE" {
        // A non-2xx INVITE final awaits its hop-ACK (§17.2.1).
        arm_reject_final(st, &txn, status);
        if is_initial {
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
    }
    Ok(())
}

/// Consume this actor's next response facts up to (and including) the first
/// FINAL on its leg — provisionals before it are passed over. A `cseq_method`
/// pin scopes the observation to ONE transaction: another transaction's final
/// (a stack-automatic PRACK/CANCEL 2xx) is passed over, never returned as
/// this goal's final. The goal-arm gate guarantees a consumable exists when a
/// final-consuming goal fires.
pub(super) fn consume_final_fact(
    st: &mut ActorState<'_>,
    cseq_method: Option<&str>,
) -> Result<ResponseFact, StepError> {
    let facts: Vec<ResponseFact> =
        st.obs.with_snapshot(|s| s.leg(st.role).responses()[st.resp_seen..].to_vec());
    let other_txn = |f: &ResponseFact| {
        cseq_method.is_some_and(|m| !f.cseq_method.eq_ignore_ascii_case(m))
    };
    for (i, f) in facts.iter().enumerate() {
        if f.status >= 200 && !other_txn(f) {
            st.resp_seen += i + 1;
            if let Some(resp) = f.typed.as_deref() {
                observe_reception(st, ReceivedMessage::Response(resp));
            }
            return Ok(f.clone());
        }
    }
    Err(StepError::UnexpectedKind {
        who: st.role.to_string(),
        detail: "final-consuming goal fired with no final observed".to_string(),
    })
}

/// `ExpectResponse`: strict, fail-fast consumption of the next response fact.
pub(super) fn expect_response(
    st: &mut ActorState<'_>,
    status: u16,
    cseq_method: Option<&str>,
    body: BodyExpect,
    early: Option<EarlyId>,
    matcher: Option<&MessageTemplate>,
) -> Result<(), StepError> {
    let facts: Vec<ResponseFact> =
        st.obs.with_snapshot(|s| s.leg(st.role).responses()[st.resp_seen..].to_vec());
    // A pinned expectation is about ONE transaction; another's response is not
    // a wrong status, it is not this expectation's business at all.
    let other_txn = |f: &ResponseFact| {
        cseq_method.is_some_and(|m| !f.cseq_method.eq_ignore_ascii_case(m))
    };
    let fact = if status < 200 {
        // The NEXT response (100 Trying is transaction plumbing, skipped) must
        // be a provisional of exactly this status — a final arriving first, or
        // a different provisional, fails fast.
        let mut found = None;
        for (i, f) in facts.iter().enumerate() {
            if f.status == 100 || other_txn(f) {
                continue;
            }
            if f.status != status {
                // Accepted-delta consult (ADR-0024 §6) before the fail-fast.
                if try_accept_response_delta(st, status, f, st.resp_seen + i + 1)? {
                    return Ok(());
                }
                return Err(StepError::WrongStatus {
                    who: st.role.to_string(),
                    expected: status,
                    got: f.status,
                    reason: f.reason.clone(),
                });
            }
            st.resp_seen += i + 1;
            found = Some(f.clone());
            break;
        }
        found
    } else {
        // Provisionals before the expected final are passed over.
        let mut found = None;
        for (i, f) in facts.iter().enumerate() {
            if f.status < 200 || other_txn(f) {
                continue;
            }
            if f.status != status {
                // Accepted-delta consult (ADR-0024 §6) before the fail-fast.
                if try_accept_response_delta(st, status, f, st.resp_seen + i + 1)? {
                    return Ok(());
                }
                return Err(StepError::WrongStatus {
                    who: st.role.to_string(),
                    expected: status,
                    got: f.status,
                    reason: f.reason.clone(),
                });
            }
            st.resp_seen += i + 1;
            found = Some(f.clone());
            break;
        }
        found
    };
    let Some(fact) = fact else {
        return Err(StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "ExpectResponse fired with no consumable response observed".to_string(),
        });
    };
    // The reception is satisfied: hand the message to the observer BEFORE the
    // goal's own assertions, so an installed hook sees it whether or not they
    // pass.
    if let Some(resp) = fact.typed.as_deref() {
        observe_reception(st, ReceivedMessage::Response(resp));
    }
    if let Some(id) = early {
        if fact.early_tag.as_deref() != Some(id) {
            return Err(StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!(
                    "ExpectResponse: fork mismatch — expected early id {id:?}, got tag {:?}",
                    fact.early_tag
                ),
            });
        }
    }
    confront_body_expect(
        st,
        body,
        fact.body_len,
        fact.body_is_sdp,
        Some(fact.status),
        &fact.cseq_method,
        false,
    );
    if let Some(tmpl) = matcher {
        let Some(resp) = &fact.typed else {
            return Err(StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: "ExpectResponse matcher: the typed response was not retained".to_string(),
            });
        };
        tmpl.match_inbound(&SipMessage::Response(resp.as_ref().clone()), &MatchOpts::default()).map_err(
            |m| StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!("response did not match its template: {m}"),
            },
        )?;
    }
    Ok(())
}

/// `ExpectRequest`: consume ONE parked request of this kind into the actor's
/// bound transaction; the matcher runs at consume time on the parked
/// transaction's request. WHICH one is [`select_parked`] — the step's `rank`,
/// then its [`BodyExpect`], then arrival order.
pub(super) fn expect_request(
    st: &mut ActorState<'_>,
    kind: &RequestKind,
    body: BodyExpect,
    rank: Option<usize>,
    matcher: Option<&MessageTemplate>,
) -> Result<(), StepError> {
    let of_kind = st.consumed_requests.get(kind).copied().unwrap_or(0);
    let Some(idx) = select_parked(&st.parked, kind, body, rank, of_kind) else {
        let detail = match (kind, st.parked_initial_consumed) {
            (RequestKind::Initial, Some(consumed)) => format!(
                "ExpectRequest: the parked initial INVITE was consumed by an automatic ({consumed})"
            ),
            _ => "ExpectRequest fired with no matching parked request".to_string(),
        };
        return Err(StepError::UnexpectedKind { who: st.role.to_string(), detail });
    };
    let entry = st.parked.remove(idx);
    *st.consumed_requests.entry(*kind).or_default() += 1;
    let req = entry.txn.request();
    // Same contract as `expect_response`: observed before the assertions.
    observe_reception(st, ReceivedMessage::Request(req));
    let (body_len, body_is_sdp) = body_facts(req);
    let method = req.method().as_str().to_string();
    confront_body_expect(
        st,
        body,
        body_len,
        body_is_sdp,
        None,
        &method,
        matches!(kind, RequestKind::Initial),
    );
    if let Some(tmpl) = matcher {
        entry.txn.expect_template(tmpl, &MatchOpts::default()).map_err(|m| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!("request did not match its template: {m}"),
            }
        })?;
    }
    if matches!(kind, RequestKind::Cancel) {
        // The CANCEL hop is already answered (its 200 stays a stack automatic,
        // sent at park time); the binding stays the INVITE the CANCEL targets,
        // so the following scripted 487 rides the BOUND INVITE transaction.
        return Ok(());
    }
    st.bound = Some(entry.txn);
    Ok(())
}

/// Confront a reception goal's [`BodyExpect`] with the body that arrived: a
/// miss records [`Observation::BodyExpectMiss`] and the run continues — the
/// miss is divergence DATA the report side classifies, never a step failure
/// that would delete the case's other records. `status` is `Some` for a
/// response (`method` its CSeq method), `None` for a request.
pub(super) fn confront_body_expect(
    st: &mut ActorState<'_>,
    body: BodyExpect,
    body_len: usize,
    body_is_sdp: bool,
    status: Option<u16>,
    method: &str,
    initial: bool,
) {
    if body.satisfied_by(body_len, body_is_sdp) {
        return;
    }
    st.obs.record(
        Observation::BodyExpectMiss {
            leg: st.role,
            step: st.goals.position(),
            expected: body.label(),
            body_len,
            body_is_sdp,
            status,
            method: method.to_string(),
            initial,
        },
        Instant::now(),
    );
}
