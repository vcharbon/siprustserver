//! CRinging phase: the transfer target C is being dialed — C's provisionals,
//! its initial answer (→ c-realign), and the failure / no-answer terminals.

use b2bua_sdk::provisional::absorbed_provisional_actions;
use b2bua_sdk::sm_rule;
use call::{CdrEventType, Direction, LegState, TransferPhase};
use sip_message::Method;

use super::{state, Phase, TRANSFER_MACHINE};
use crate::rules::refer_transfer::notify::{
    notify, SUB_STATE_TERMINATED_NORESOURCE, SUB_STATE_TERMINATED_TIMEOUT,
};
use crate::rules::refer_transfer::ok;
use crate::rules::Terminal;
use b2bua_sdk::model::{
    Body, Effect, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, TimerDelay,
};
use call::TimerKey;

/// Whether the event is C's response to the INVITE that dialled it: one at
/// another number answers a later request of C's dialog, not that INVITE.
fn from_c_to_its_invite(ctx: &RuleContext) -> bool {
    let from_c = state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id);
    let cseq = ctx.response().map(|r| i64::from(r.cseq().seq()));
    from_c && cseq.is_some() && cseq == ctx.source_leg().and_then(|l| l.dialled_cseq())
}

/// transfer-c-1xx-absorb — C 1xx. The referrer's peer is answered, so C's
/// provisional is shown to no one: C keeps what it is owed (`Early`, a PRACK
/// on a reliable one, the CDR event, accounted once per distinct status). The
/// referrer hears of no progress: its subscription carries the immediate
/// NOTIFY and the final one alone (RFC 3515 §3.10).
pub(super) fn c_1xx_absorb() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-1xx-absorb",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRinging ],
        transitions: [],
        effects: [],
        matcher: Match::response()
            .method("INVITE")
            .status_class(1)
            .direction(Direction::FromB)
            .filter(from_c_to_its_invite),
        handle: |ctx| {
            let st = state(ctx)?;
            let resp = ctx.response()?;
            let mut actions = absorbed_provisional_actions(ctx);
            if st.last_c_leg_provisional_status == Some(resp.status()) {
                actions.retain(|a| !matches!(a, RuleAction::AddCdrEvent { .. }));
                return ok(actions);
            }
            let mut new_state = st.clone();
            new_state.last_c_leg_provisional_status = Some(resp.status());
            actions.push(RuleAction::SetTransfer { state: Some(new_state) });
            ok(actions)
        },
    }
}

/// transfer-c-200-initial — C answers its initial INVITE.
pub(super) fn c_200_initial() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-200-initial",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRinging ],
        transitions: [ Phase::CRinging => Phase::CRealigning ],
        effects: [
            Effect::Originate { method: Method::Ack, label: "ACK → C (answer initial INVITE)" },
            Effect::Originate { method: Method::Notify, label: "NOTIFY terminated → referrer" },
            Effect::Originate { method: Method::Invite, label: "re-INVITE → C (c-realign, A's SDP)" },
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, scope: call::TimerScopeKind::Leg, label: "arm c-realign re-INVITE watchdog" },
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, scope: call::TimerScopeKind::Call, label: "cancel subscription-expiry" },
            Effect::GuardTimer { timer: call::TimerType::NoAnswer, scope: call::TimerScopeKind::Leg, label: "cancel C no-answer" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(2)
            .direction(Direction::FromB)
            .leg_states(&[LegState::Trying, LegState::Early])
            .filter(from_c_to_its_invite),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let c_leg_id = st.c_leg_id.clone()?;

            // Capture C's 200 SDP (drives the a-realign re-INVITE).
            let c_initial_sdp = resp.sdp().map(<[u8]>::to_vec);
            // A's SDP for the c-realign re-INVITE-C offer.
            let Some(a_leg) = ctx.caller_invite() else {
                return Some(RuleHandleResult::unrecoverable());
            };
            let a_sdp = a_leg.sdp().unwrap_or_default();
            // A's lines describe the offer only where it is her whole body.
            let a_offer = Body::from_leg(a_sdp.to_vec(), ctx.call.a_leg().leg_id.to_string());
            let a_offer = if a_sdp == a_leg.body().as_ref() {
                a_offer.described_by(a_leg.headers())
            } else {
                a_offer
            };

            let mut new_state = st.clone();
            new_state.phase = TransferPhase::CRealigning;
            new_state.c_initial_sdp = c_initial_sdp;

            let mut actions = vec![
                RuleAction::ConfirmDialog { leg_id: c_leg_id.clone() },
                RuleAction::AckLeg { leg_id: c_leg_id.clone(), body: None },
            ];
            actions.push(notify(&st, SUB_STATE_TERMINATED_NORESOURCE, 200, "OK"));
            actions.extend([
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferSubscriptionExpiry)),
                RuleAction::cancel(TimerKey::leg(call::TimerType::NoAnswer, &c_leg_id)),
                RuleAction::arm(
                    TimerKey::leg(call::TimerType::ReferReinviteAnswer, &c_leg_id),
                    TimerDelay::secs(ctx.config.refer_reinvite_answer_sec),
                ),
                RuleAction::SendReinvite {
                    leg_id: c_leg_id.clone(),
                    body: Some(a_offer),
                    add_headers: vec![],
                },
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Answer,
                    leg_id: c_leg_id,
                    status_code: Some(200),
                    reason: None,
                },
                RuleAction::SetTransfer { state: Some(new_state) },
            ]);
            ok(actions)
        },
    }
}

/// transfer-c-fail-initial — C's initial INVITE 3xx–6xx.
pub(super) fn c_fail_initial() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-fail-initial",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRinging ],
        transitions: [ Phase::CRinging => Terminal ],
        effects: [
            Effect::Originate { method: Method::Notify, label: "NOTIFY terminated (C failed) → referrer" },
            Effect::Originate { method: Method::Bye, label: "BYE → C (terminate failed leg)" },
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, scope: call::TimerScopeKind::Call, label: "cancel subscription-expiry" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .direction(Direction::FromB)
            .leg_states(&[LegState::Trying, LegState::Early])
            .filter(|ctx| {
                let is_fail = ctx.response().map(|r| r.status() >= 300).unwrap_or(false);
                is_fail && from_c_to_its_invite(ctx)
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let mut actions = Vec::new();
            actions.push(notify(&st, SUB_STATE_TERMINATED_NORESOURCE, resp.status(), resp.reason()));
            actions.extend([
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferSubscriptionExpiry)),
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)),
            ]);
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: c_leg_id.clone(),
                    status_code: Some(resp.status() as i64),
                    reason: Some(resp.reason().to_string()),
                });
                actions.push(RuleAction::TerminateLeg {
                    leg_id: c_leg_id,
                    end: b2bua_sdk::model::LegEnd::InviteRejected,
                });
            }
            actions.push(RuleAction::SetTransfer { state: None });
            ok(actions)
        },
    }
}

/// transfer-c-no-answer — C no-answer timer (beats CORE no-answer).
pub(super) fn c_no_answer() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-no-answer",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRinging ],
        transitions: [ Phase::CRinging => Terminal ],
        effects: [
            Effect::Originate { method: Method::Notify, label: "NOTIFY terminated;timeout → referrer" },
            Effect::Originate { method: Method::Bye, label: "BYE → C (no answer)" },
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, scope: call::TimerScopeKind::Call, label: "cancel subscription-expiry" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::NoAnswer)
            .filter(|ctx| {
                let timer_leg = ctx.timer_key().and_then(TimerKey::leg_id);
                state(ctx).and_then(|s| s.c_leg_id.as_deref()).is_some()
                    && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == timer_leg
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let c_leg_id = st.c_leg_id.clone()?;
            let mut actions = Vec::new();
            actions.push(notify(&st, SUB_STATE_TERMINATED_TIMEOUT, 408, "Request Timeout"));
            actions.extend([
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: c_leg_id.clone(),
                    status_code: None,
                    reason: Some("no_answer_timeout".to_string()),
                },
                RuleAction::DestroyLeg { leg_id: c_leg_id, headers: vec![] },
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferSubscriptionExpiry)),
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)),
                RuleAction::SetTransfer { state: None },
            ]);
            ok(actions)
        },
    }
}
