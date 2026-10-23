//! The CORE_LAYER failover resolution: the async `/call/failure` round-trip
//! folds its decision back as a `call-failure-result` internal event, and one
//! rule per outcome applies it — a fresh b-leg, the original failure let
//! stand, a decision-authored reject or redirect (ADR-0017).

use call::TerminationCause;
use call::TimeoutKind;

use b2bua_sdk::header_update::final_adds;
use b2bua_sdk::model::{Match, RuleAction, RuleDefinition};

use super::core_rules::{ok, rule};
use super::route_fold::{
    fold_lands_on_going_away_call, parse_header_updates, parse_route_fold, parse_service_ext,
    route_fold_parity_actions,
};

/// What ended the call when a failure consult lets the failure stand (the
/// `terminate` fold): the failed leg's final where it drew one, else the
/// deadline the consult was raised for — a leg that never answered, or a
/// transaction that drew nothing. An unanswered consult and an explicit
/// relay decision both land here; the record does not tell them apart (the
/// decision log does: the relay is marked, the unanswered consult is not).
fn failure_let_stand_cause(payload: &serde_json::Value) -> TerminationCause {
    if payload.get("status").is_some() {
        return TerminationCause::RemoteFinal;
    }
    match payload.get("origin").and_then(|v| v.as_str()) {
        Some("no_answer_timeout") => TerminationCause::Timeout(TimeoutKind::NoAnswer),
        _ => TerminationCause::Timeout(TimeoutKind::Transaction),
    }
}

/// The caller's final when a failure with no final of its own is let stand,
/// authored in the terminating turn so it precedes the record's cut: 480 for
/// a callee that never answered, 408 for a transaction that drew nothing.
fn let_stand_final(payload: &serde_json::Value) -> RuleAction {
    let (status, reason) = match payload.get("origin").and_then(|v| v.as_str()) {
        Some("no_answer_timeout") => (480, "Temporarily Unavailable"),
        _ => (408, "Request Timeout"),
    };
    RuleAction::RespondToALeg {
        status,
        reason: reason.into(),
        header_updates: vec![],
        contacts: vec![],
    }
}

/// A redirect Contact's stated q-value, read as written (RFC 3261 §20.10).
fn q_value(q: &serde_json::Value) -> Option<f32> {
    q.to_string().parse().ok()
}

/// The failover resolution rules, in registration (priority) order.
pub(super) fn failover_rules() -> [RuleDefinition; 4] {
    [failover_create_leg(), failover_terminate(), failover_reject(), failover_redirect()]
}

/// `failover` → a fresh b-leg toward the new destination under the
/// failover's attempt (A's INVITE snapshot; the relay_first_18x slice
/// survives: the To-tag stays the first 180's).
fn failover_create_leg() -> RuleDefinition {
    rule(
        "failover-create-leg",
        &[],
        Match::internal_event().topic("call-failure-result").outcome("failover"),
        |ctx| {
            // Fold landed on a going-away call: the caller already
            // holds its final — drop whole, no leg toward a caller-less
            // callee (see `fold_lands_on_going_away_call`).
            if fold_lands_on_going_away_call(ctx) {
                return ok(vec![]);
            }
            let payload = match ctx.event {
                b2bua_sdk::event::CallEvent::InternalEvent { payload, .. } => payload,
                _ => return None,
            };
            // ── failover/initial-route parity ────────────────────────────
            // The same decision response must be honored the same way on
            // both paths, so the reroute applies what `apply_route` applies
            // (one shared parser + parity-action builder, also used by the
            // `release-reroute` fold): features (incl. the GlobalDuration
            // re-arm), service_ext, subscriptions, update_body, and the
            // limiter state (the route's admitted set replaced the call's).
            let fold = parse_route_fold(payload)?;
            let mut actions = route_fold_parity_actions(&fold, ctx);
            let leg_body = fold.leg_body(ctx);
            actions.push(RuleAction::CreateLeg {
                attempt: Some(fold.attempt(call::AttemptCause::Failover, payload)),
                destination: fold.destination,
                new_ruri: fold.new_ruri,
                new_from: fold.new_from,
                new_to: fold.new_to,
                callback_context: fold.callback_context,
                body_override: leg_body,
                header_updates: fold.header_updates,
                header_adds: fold.header_adds,
                kind: None,
            });
            ok(actions)
        },
    )
}

/// `terminate` (or backend error) → relay the original failure to the
/// caller (response path; the no-answer path carries no status) and tear
/// the call down.
fn failover_terminate() -> RuleDefinition {
    rule(
        "failover-terminate",
        &[],
        Match::internal_event().topic("call-failure-result").outcome("terminate"),
        |ctx| {
            // Fold landed on a going-away call: the caller already
            // holds its final — no relayed failure, no re-termination.
            if fold_lands_on_going_away_call(ctx) {
                return ok(vec![]);
            }
            let payload = match ctx.event {
                b2bua_sdk::event::CallEvent::InternalEvent { payload, .. } => payload,
                _ => return None,
            };
            let mut actions = Vec::new();
            match payload.get("status").and_then(|v| v.as_u64()).and_then(|s| u16::try_from(s).ok())
            {
                Some(status) => {
                    let reason = payload
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Server Internal Error")
                        .to_string();
                    actions.push(RuleAction::RelayFailureToALeg { status, reason });
                }
                // A timeout let stand: the caller's final is this turn's,
                // under the termination record's cut.
                None => actions.push(let_stand_final(payload)),
            }
            actions.push(RuleAction::BeginTermination {
                reason: Some("failover-declined".into()),
                cause: failure_let_stand_cause(payload),
                by_leg: payload
                    .get("failed_leg_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            });
            ok(actions)
        },
    )
}

/// `reject` → the plan declined to fail over and authored its own final
/// failure (code/reason/headers, e.g. a `Reason:` header). Send it to A and
/// tear the call down. Port-parallel of `failover-terminate` (ADR-0017).
fn failover_reject() -> RuleDefinition {
    rule(
        "failover-reject",
        &[],
        Match::internal_event().topic("call-failure-result").outcome("reject"),
        |ctx| {
            // Fold landed on a going-away call: no second final on
            // the a-leg's completed transaction (RFC 3261 §17.2.1).
            if fold_lands_on_going_away_call(ctx) {
                return ok(vec![]);
            }
            let payload = match ctx.event {
                b2bua_sdk::event::CallEvent::InternalEvent { payload, .. } => payload,
                _ => return None,
            };
            let mut actions = Vec::new();
            // A reject seeds its service slices exactly as a route does.
            let service_ext = parse_service_ext(payload);
            if !service_ext.is_empty() {
                actions.push(RuleAction::MergeCallExt { ext: service_ext });
            }
            // The limiter chain's terminal 486 and the engine's stated
            // refusal are the stack's own finals, not decisions.
            let stack_authored = crate::decision_log::stack_authored(payload);
            let cause = if stack_authored {
                TerminationCause::Admission
            } else {
                TerminationCause::DecisionReject
            };
            let ending = RuleAction::BeginTermination {
                reason: Some("failover-reject".into()),
                cause,
                by_leg: None,
            };
            let header_updates = parse_header_updates(payload);
            // A decided reject applies no route: the failed route's stated
            // headers leave the call, and the reject's own adds ride its
            // final, yielding to the lines the final carries. The stack's
            // own refusal is no decision: the route's headers stay.
            if !stack_authored {
                actions.push(RuleAction::SetStatedHeaders {
                    headers: final_adds(payload.get("update_headers")),
                });
            }
            // The initial path's guard: a code that is no non-2xx final
            // rejects nothing and draws the plain server error, which keeps
            // the decision's headers and none of the failing peer's lines.
            let stated = payload.get("code").and_then(|v| v.as_u64());
            let Some(status) = stated
                .map_or(Some(500), |code| u16::try_from(code).ok())
                .filter(|code| crate::decision::apply_reject::is_reject_code(*code))
            else {
                tracing::warn!(call_ref = %ctx.call_ref, code = ?stated, "reject refused: not a non-2xx final");
                actions.extend([
                    RuleAction::MergeCallExt { ext: b2bua_sdk::failure_image::no_failure_image() },
                    RuleAction::RespondToALeg {
                        status: 500,
                        reason: crate::decision::apply_reject::REFUSED_DECISION_REASON.to_string(),
                        header_updates,
                        contacts: Vec::new(),
                    },
                    ending,
                ]);
                return ok(actions);
            };
            // The initial path's reason policy: a reject stating no phrase
            // wears its code's.
            let reason = payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| b2bua_sdk::reason_phrase::default_reason(status))
                .to_string();
            actions.extend([
                RuleAction::RespondToALeg { status, reason, header_updates, contacts: vec![] },
                ending,
            ]);
            ok(actions)
        },
    )
}

/// `redirect` → the plan authored a 3xx with a Contact list. Send it to A
/// (the caller retries the targets) and tear the call down (ADR-0017).
fn failover_redirect() -> RuleDefinition {
    rule(
        "failover-redirect",
        &[],
        Match::internal_event().topic("call-failure-result").outcome("redirect"),
        |ctx| {
            // Fold landed on a going-away call: no second final on
            // the a-leg's completed transaction (RFC 3261 §17.2.1).
            if fold_lands_on_going_away_call(ctx) {
                return ok(vec![]);
            }
            let payload = match ctx.event {
                b2bua_sdk::event::CallEvent::InternalEvent { payload, .. } => payload,
                _ => return None,
            };
            // A code no 16-bit status holds is no 3xx: refused below.
            let status = payload
                .get("code")
                .and_then(|v| v.as_u64())
                .map_or(302, |code| u16::try_from(code).unwrap_or(0));
            let reason = payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("Moved Temporarily")
                .to_string();
            let header_updates = parse_header_updates(payload);
            let contacts: Vec<(String, Option<f32>)> = payload
                .get("contacts")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            let uri = c.get("uri")?.as_str()?.to_string();
                            let q = c.get("q").filter(|v| v.is_number()).and_then(q_value);
                            Some((uri, q))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut actions = Vec::new();
            // A redirect seeds its service slices exactly as a route does.
            let service_ext = parse_service_ext(payload);
            if !service_ext.is_empty() {
                actions.push(RuleAction::MergeCallExt { ext: service_ext });
            }
            // A redirect applies no route: as a reject's, its adds replace
            // the route's set, refused or not.
            actions.push(RuleAction::SetStatedHeaders {
                headers: final_adds(payload.get("update_headers")),
            });
            if crate::decision::apply_reject::is_redirect_code(status) {
                // One whose target does not read is refused at the seam with
                // the plain server error, keeping the decision's headers.
                actions.push(RuleAction::RespondToALeg {
                    status,
                    reason,
                    header_updates,
                    contacts,
                });
            } else {
                // A redirect that is not a 3xx is refused with the plain
                // server error, as an unreadable target's refusal is: the
                // failing peer's image is cleared, the decision's headers
                // kept.
                tracing::warn!(call_ref = %ctx.call_ref, code = status, "redirect refused: not a 3xx");
                actions.extend([
                    RuleAction::MergeCallExt { ext: b2bua_sdk::failure_image::no_failure_image() },
                    RuleAction::RespondToALeg {
                        status: 500,
                        reason: crate::decision::apply_reject::REFUSED_DECISION_REASON.to_string(),
                        header_updates,
                        contacts: Vec::new(),
                    },
                ]);
            }
            // `DecisionReject` on either arm, as for an unreadable target on
            // this path: the call had a routed leg, which the initial
            // INVITE's refusal (`Admission`) never had.
            actions.push(RuleAction::BeginTermination {
                reason: Some("failover-redirect".into()),
                cause: TerminationCause::DecisionReject,
                by_leg: None,
            });
            ok(actions)
        },
    )
}
