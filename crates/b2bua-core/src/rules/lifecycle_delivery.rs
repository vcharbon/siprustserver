//! The end of a turn's lifecycle (ADR-0042): the open attempts are settled
//! against the legs and the call, every timer whose scope ended — an ended
//! attempt's, the caller's wait once its INVITE is answered — is retired, and
//! the turn's lifecycle events — each attempt opened or ended,
//! each leg that ended — reach every active service, whichever rule claimed
//! the turn. A service handles one with a machine-bound rule matching
//! [`MatchKind::Lifecycle`]; its handler may arm or cancel its own service
//! timers, move its own machine's cursor and write call ext. A machine the
//! turn deactivated, or one on a call the turn terminated, still hears the
//! events the turn caused, for its ext writes alone; those reach the call's
//! CDR, which is written from the call as the turn lands, after this close.

use std::collections::BTreeMap;

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::lifecycle::{LifecycleEvent, RouteAnswer};
use call::{
    AttemptId, Call, CallModelState, Direction, LegState, MachineId, StateLabel, TimerKey,
    TimerType,
};

use crate::effects::{CriticalStateEffect, HandlerResult};
use crate::turn_notes::TurnNotes;

use super::actions::ActionExecutor;
use super::executor::{
    check_declared_effects, check_declared_transition, machine_active, record_cursor_moves,
};
use b2bua_sdk::model::{MatchKind, RuleAction, RuleCall, RuleContext, RuleDefinition};

/// What the turn's lifecycle is read against: the call as the turn found it.
#[derive(Debug, Clone, Default)]
pub struct TurnStart {
    /// Each attempt the call held, and whether it was open.
    attempts: Vec<(AttemptId, bool)>,
    /// The legs that had already ended.
    ended_legs: Vec<String>,
    /// Every machine's cursor.
    cursors: BTreeMap<MachineId, StateLabel>,
}

impl TurnStart {
    /// The turn starts from `call`.
    pub fn of(call: &Call) -> Self {
        TurnStart {
            attempts: call.attempts.iter().map(|a| (a.id, a.is_open())).collect(),
            ended_legs: legs(call)
                .filter(|l| l.state() == LegState::Terminated)
                .map(|l| l.leg_id.clone())
                .collect(),
            cursors: call.sm_cursors.clone(),
        }
    }

    /// The turn creates the call: nothing was there before it.
    pub fn unborn() -> Self {
        Self::default()
    }
}

/// Close the turn that started at `start` and left `result`: settle the open
/// attempts, retire the timers of each attempt that ended, deliver the turn's
/// lifecycle events to the active services of `rules`, then retire every timer
/// whose scope ended.
pub fn close_turn(
    rules: &[RuleDefinition],
    start: &TurnStart,
    mut result: HandlerResult,
    exec: &mut ActionExecutor,
) -> HandlerResult {
    for (id, _) in call::helpers::settle_attempts(&mut result.call) {
        crate::attempts::retire(&mut result.call, &mut result.effects, id);
    }
    let opened = std::mem::take(&mut result.effects.opened_attempts);
    for event in events(start, &result.call, opened) {
        deliver(rules, start, event, &mut result, exec);
    }
    // Leaving a scope retires it, a timer a handler armed under an ended
    // attempt included.
    let call = &result.call;
    let ended: Vec<TimerKey> = call
        .timers
        .iter()
        .filter(|t| call::helpers::scope_ended(call, &t.key))
        .map(|t| t.key.clone())
        .collect();
    for t in call::helpers::retire_timers(&mut result.call, |t| ended.contains(&t.key)) {
        result.effects.critical.push(CriticalStateEffect::CancelTimer { key: t.key });
    }
    result
}

/// The turn's lifecycle events, in order: by attempt, its start then its end;
/// then each leg that ended, the originator's first.
fn events(
    start: &TurnStart,
    call: &Call,
    mut opened: Vec<(AttemptId, RouteAnswer)>,
) -> Vec<LifecycleEvent> {
    let mut events = Vec::new();
    for attempt in &call.attempts {
        let was = start.attempts.iter().find(|(id, _)| *id == attempt.id);
        if was.is_none() {
            let route = opened
                .iter_mut()
                .find(|(id, _)| *id == attempt.id)
                .map(|(_, route)| std::mem::take(route))
                .unwrap_or_default();
            events.push(LifecycleEvent::AttemptStarted { attempt: attempt.id, route });
        }
        let was_open = was.is_none_or(|(_, open)| *open);
        if let (Some(outcome), true) = (attempt.outcome, was_open) {
            events.push(LifecycleEvent::AttemptEnded { attempt: attempt.id, outcome });
        }
    }
    for leg in legs(call) {
        if leg.state() == LegState::Terminated && !start.ended_legs.contains(&leg.leg_id) {
            events.push(LifecycleEvent::LegEnded { leg: leg.leg_id.clone(), cause: leg.end() });
        }
    }
    events
}

/// Deliver `event` to every lifecycle rule of a service whose machine is
/// active, or was active when the turn started and the turn deactivated it.
fn deliver(
    rules: &[RuleDefinition],
    start: &TurnStart,
    event: LifecycleEvent,
    result: &mut HandlerResult,
    exec: &mut ActionExecutor,
) {
    let source_leg = match &event {
        LifecycleEvent::LegEnded { leg, .. } => leg.clone(),
        _ => result.call.a_leg.leg_id.clone(),
    };
    let kind = event.kind();
    let event = CallEvent::Lifecycle(event);
    for rule in rules.iter().filter(|r| r.matcher.kind == MatchKind::Lifecycle) {
        let Some(machine) = rule.machine.as_ref() else {
            continue;
        };
        let active = machine_active(rule, &result.call);
        let deactivated = !result.call.sm_cursors.contains_key(machine)
            && start.cursors.get(machine).is_some_and(|c| rule.active_states.contains(c));
        if !active && !deactivated {
            continue;
        }
        // A terminated call is removed as the turn lands: nothing is armed on it.
        let live = active && result.call.state != CallModelState::Terminated;
        let sub = {
            let ctx = RuleContext {
                call: RuleCall::new(&result.call),
                call_ref: &result.call.call_ref,
                event: &event,
                source_leg_id: &source_leg,
                direction: Direction::FromA,
                now_ms: exec.now_ms,
                config: exec.config,
                discharged: None,
                resolved: &call::helpers::UNRESOLVED,
            };
            if !rule.matcher.accepts_columns(&ctx) || !rule.matcher.filter.is_none_or(|f| f(&ctx)) {
                continue;
            }
            let Some(outcome) = (rule.handle)(&ctx) else {
                continue;
            };
            for d in &outcome.diagnostics {
                tracing::warn!(call_ref = %result.call.call_ref, rule = %rule.id, detail = %d, "rule refused an input");
            }
            let notes = &mut exec.turn.notes;
            let actions = admitted(rule, machine, live, outcome.actions, &result.call, notes);
            check_declared_effects(rule, &actions, &result.call, notes);
            exec.execute(&actions, &result.call, &ctx)
        };
        if live {
            let after = &sub.call.sm_cursors;
            check_declared_transition(rule, &result.call, after, false, &mut exec.turn.notes);
        }
        let fact = || format!("{} {kind}", rule.id);
        exec.turn.notes.trace(&sub.call, "rule.lifecycle", exec.now_ms, fact);
        let moved_from = result.call.clone();
        result.call = sub.call;
        result.effects.extend(sub.effects);
        record_cursor_moves(rule, &moved_from, &result.call, exec.now_ms, &mut exec.turn.notes);
    }
}

/// The actions a lifecycle handler may take: call-ext writes, and while its
/// machine is live, its own cursor moves and its own service timers. Any
/// other action is an authoring bug: it is dropped and noted as a defect on
/// `call`. A deactivated machine's bookkeeping is dropped: the machine is gone.
fn admitted(
    rule: &RuleDefinition,
    machine: &MachineId,
    live: bool,
    actions: Vec<RuleAction>,
    call: &Call,
    notes: &mut TurnNotes,
) -> Vec<RuleAction> {
    actions
        .into_iter()
        .filter(|action| {
            let own = match action {
                RuleAction::MergeCallExt { .. } => return true,
                RuleAction::SetState { machine: m, .. } | RuleAction::ClearState { machine: m } => {
                    m == machine
                }
                RuleAction::ScheduleTimer { key, .. } | RuleAction::CancelTimer { key } => {
                    matches!(&key.timer_type, TimerType::Service { service_id, .. } if service_id == machine)
                }
                _ => false,
            };
            if own {
                return live;
            }
            let detail = format!(
                "rule '{}' handled a lifecycle event with a {:?} action (a lifecycle \
                 handler writes call ext, its own machine's cursor and its own timers only); \
                 action dropped: {action:?}",
                rule.id,
                action.effect_kind(),
            );
            notes.defect(crate::turn_notes::LIFECYCLE_OUT_OF_SCOPE, detail, call);
            false
        })
        .collect()
}

/// The originator's leg, then every leg this stack dialled.
fn legs(call: &Call) -> impl Iterator<Item = &call::Leg> {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter())
}
