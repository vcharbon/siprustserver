//! Leaving a scope retires it ([`Scope`]): every ladder the scope owes ends —
//! its rung and give-up leave the ledger and the live driver, and what it
//! repeated stops riding the replicated body — and every timer guarding the
//! scope is disarmed: a leg's timers with the leg, an attempt's as it ends, a
//! service machine's as it deactivates, the call's as its termination
//! begins. No rule cancels a timer its scope already took.

use call::helpers::Scope;
use call::{Call, TimerType};

use crate::effects::HandlerEffects;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Retire `scope`. Ladders are keyed by the books, so a ladder that
    /// already ceased still loses the give-up it left armed; at
    /// [`Scope::Call`] a ladder timer the books no longer name goes too.
    pub(super) fn retire(&self, call: &mut Call, fx: &mut HandlerEffects, scope: Scope<'_>) {
        let mut obligations = call::helpers::obligations_in(call, &scope);
        if scope == Scope::Call {
            for o in call.timers.iter().filter_map(|t| t.key.obligation_of()) {
                if !obligations.contains(o) {
                    obligations.push(o.clone());
                }
            }
        }
        for obligation in obligations {
            self.scrub_timers(call, fx, &obligation);
            *call = call::helpers::clear_retained(call.clone(), &obligation);
        }
        match scope {
            Scope::Leg(leg_id) => self.scrub_where(call, fx, |t| t.leg_id() == Some(leg_id)),
            Scope::Attempt(id) => crate::attempts::retire(call, fx, id),
            Scope::Machine(machine) => self.scrub_where(call, fx, |t| {
                matches!(t.timer_type(), TimerType::Service { service_id, .. } if service_id == machine)
            }),
            Scope::Call => self.scrub_where(call, fx, |t| !outlives_termination(t.timer_type())),
            Scope::Obligation(_) | Scope::Transaction { .. } | Scope::Provisionals => {}
        }
    }
}

/// Whether a timer outlives the start of the call's termination: the
/// teardown's own safety net, the limiter refresh of a call that holds its
/// admission until it ends, and the deadline of an answer the call awaits off
/// its turn (ADR-0039), whose answer still settles what the request holds.
fn outlives_termination(timer_type: &TimerType) -> bool {
    matches!(
        timer_type,
        TimerType::TerminatingTimeout
            | TimerType::LimiterRefresh
            | TimerType::ServiceHttpAnswer { .. }
            | TimerType::ServiceAdmitAnswer { .. }
            | TimerType::FailureAnswer { .. }
    )
}
