//! Leaving a scope retires it ([`Scope`]): every ladder the scope owes ends —
//! its timers leave the ledger and the live driver, and what it
//! repeated stops riding the replicated body — and every timer guarding the
//! scope is disarmed: a leg's timers with the leg, an attempt's as it ends, a
//! service machine's as it deactivates, the call's as its termination
//! begins. No rule cancels a timer its scope already took. A 2xx a BYE of
//! this stack waits behind (RFC 3261 §15) outlives the BYE's leg being
//! destroyed and the call's termination: its ACK or its give-up releases
//! that BYE.

use call::helpers::Scope;
use call::{Call, DialogId, Obligation, QueuedRequest, TimerType};

use crate::effects::HandlerEffects;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Retire `scope`. Ladders are keyed by the books, so a 2xx's ladder that
    /// already ceased still loses the give-up it left armed; at
    /// [`Scope::Call`] a ladder timer the books no longer name goes too.
    pub(super) fn retire(&mut self, call: &mut Call, fx: &mut HandlerEffects, scope: Scope<'_>) {
        let at_call = scope == Scope::Call;
        self.retire_keeping(call, fx, scope, at_call);
    }

    /// Retire `leg_id`'s scope as this stack BYEs it: the 2xx its BYE waits
    /// behind keeps its ladder until its ACK or its give-up (§15).
    pub(super) fn retire_byed_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
    ) {
        self.retire_keeping(call, fx, Scope::Leg(leg_id), true);
    }

    /// Retire `scope`, keeping the 2xx a held BYE waits behind where
    /// `keeps_held`.
    fn retire_keeping(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        scope: Scope<'_>,
        keeps_held: bool,
    ) {
        let mut obligations = call::helpers::obligations_in(call, &scope);
        if scope == Scope::Call {
            for o in call.timers.iter().filter_map(|t| t.key.obligation_of()) {
                if !obligations.contains(o) {
                    obligations.push(o.clone());
                }
            }
        }
        let held: Vec<Obligation> =
            obligations.iter().filter(|o| bye_behind(call, o).is_some()).cloned().collect();
        for obligation in obligations {
            if keeps_held && held.contains(&obligation) {
                continue;
            }
            let bye = bye_behind(call, &obligation);
            self.scrub_timers(call, fx, &obligation);
            *call = call::helpers::clear_retained(call.clone(), &obligation);
            // Discharged — its ACK arrived — the BYE behind it leaves; with its
            // leg the dialog it would end is gone.
            if let (Scope::Obligation(_), Some((leg_id, id, bye))) = (&scope, bye) {
                self.send_queued_bye(call, fx, &leg_id, &id, bye);
            }
        }
        match scope {
            Scope::Leg(leg_id) => self.scrub_where(call, fx, |t| {
                t.leg_id() == Some(leg_id)
                    && (!keeps_held || t.key.obligation_of().is_none_or(|o| !held.contains(o)))
            }),
            Scope::Attempt(id) => crate::attempts::retire(call, fx, id),
            Scope::Machine(machine) => self.scrub_where(call, fx, |t| {
                matches!(t.timer_type(), TimerType::Service { service_id, .. } if service_id == machine)
            }),
            Scope::Call => self.scrub_where(call, fx, |t| {
                !outlives_termination(t.timer_type())
                    && t.key.obligation_of().is_none_or(|o| !held.contains(o))
            }),
            Scope::Obligation(_) | Scope::Transaction { .. } | Scope::Provisionals => {}
        }
    }

    /// The 2xx `obligation` names was given up on (RFC 3261 §13.3.1.4): the
    /// BYE that waited behind its ACK leaves now (§15).
    pub(super) fn release_bye_at_give_up(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        obligation: &Obligation,
    ) {
        if let Some((leg_id, id, bye)) = bye_behind(call, obligation) {
            *call = call::helpers::clear_retained(call.clone(), obligation);
            self.send_queued_bye(call, fx, &leg_id, &id, bye);
        }
    }
}

/// The BYE of this stack waiting behind the ACK of the 2xx `obligation`
/// names, with the leg and dialog it rides.
pub(super) fn bye_behind(
    call: &Call,
    obligation: &Obligation,
) -> Option<(String, DialogId, QueuedRequest)> {
    let Obligation::AckOf2xx { leg, dialog_tag, cseq } = obligation else { return None };
    let d = call::helpers::find_leg(call, leg)?.dialogs.iter().find(|d| {
        call::helpers::answered_2xx(d)
            .is_some_and(|u| u.dialog_tag == *dialog_tag && u.cseq == *cseq)
    })?;
    let bye = d.answered_invite.as_ref()?.invite.as_ref()?.bye_behind_ack.clone()?;
    Some((leg.clone(), d.id(), bye))
}

/// Whether a timer outlives the start of the call's termination: the
/// teardown's own safety net, the wait of a BYE queued behind this stack's own
/// NOTIFY, the limiter refresh of a call that holds its
/// admission until it ends, and the deadline of an answer the call awaits off
/// its turn (ADR-0039), whose answer still settles what the request holds.
fn outlives_termination(timer_type: &TimerType) -> bool {
    matches!(
        timer_type,
        TimerType::TerminatingTimeout
            | TimerType::NotifyAnswerCap
            | TimerType::LimiterRefresh
            | TimerType::ServiceHttpAnswer { .. }
            | TimerType::ServiceAdmitAnswer { .. }
            | TimerType::FailureAnswer { .. }
            | TimerType::ReleaseAnswer { .. }
    )
}
