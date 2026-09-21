//! CRinging phase: the transfer target C is being dialed — progress NOTIFYs
//! toward the referrer, C's initial answer (→ c-realign), and the failure /
//! no-answer terminals.

use b2bua_sdk::provisional::absorbed_provisional_actions;
use b2bua_sdk::sm_rule;
use call::{CdrEventType, Direction, LegState, TransferPhase};
use sip_message::Method;

use super::{state, timer_id, Phase, TRANSFER_MACHINE};
use crate::rules::model::{Effect, Match, RuleAction, RuleDefinition, TimerDelay};
use crate::rules::refer_transfer::notify::{
    notify, SUB_STATE_ACTIVE_60, SUB_STATE_TERMINATED_NORESOURCE, SUB_STATE_TERMINATED_TIMEOUT,
};
use crate::rules::refer_transfer::ok;
use crate::rules::{relay, Terminal};

/// transfer-c-1xx-to-notify — C 1xx → NOTIFY active (deduped). The referrer's
/// peer is answered, so C's provisional is shown to no one: C keeps what it is
/// owed (`Early`, a PRACK on a reliable one, the CDR event) and the referrer
/// hears of the progress.
pub(super) fn c_1xx_to_notify() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-1xx-to-notify",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRinging ],
        transitions: [],
        effects: [
            Effect::Originate { method: Method::Notify, label: "NOTIFY active (C progress) → referrer" },
            Effect::Originate { method: Method::Prack, label: "PRACK → C (reliable 1xx, nothing shown to A)" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(1)
            .direction(Direction::FromB)
            .filter(|ctx| {
                state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let mut actions = absorbed_provisional_actions(ctx);
            // Dedupe identical repeats against the *last* status only.
            if st.last_c_leg_notified_status == Some(resp.status()) {
                return ok(actions);
            }
            let mut new_state = st.clone();
            new_state.last_c_leg_notified_status = Some(resp.status());
            actions.extend(notify(&st, SUB_STATE_ACTIVE_60, resp.status(), resp.reason()));
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
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "arm c-realign re-INVITE watchdog" },
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry" },
            Effect::GuardTimer { timer: call::TimerType::NoAnswer, label: "cancel C no-answer" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(2)
            .direction(Direction::FromB)
            .leg_states(&[LegState::Trying, LegState::Early])
            .filter(|ctx| {
                state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let c_leg_id = st.c_leg_id.clone()?;

            // Capture C's 200 SDP (drives the a-realign re-INVITE).
            let c_initial_sdp = resp.sdp().map(<[u8]>::to_vec);
            // A's SDP for the c-realign re-INVITE-C offer.
            // FIXME(refer): a delayed-offer a-INVITE leaves this empty, so the
            // re-INVITE to C makes a delayed offer its ACK never answers; source
            // A's current description (her ACK answer) instead.
            let a_leg = relay::rebuild_a_leg_invite(ctx.call.a_leg_invite());
            let a_sdp = a_leg.sdp().unwrap_or_default();

            let mut new_state = st.clone();
            new_state.phase = TransferPhase::CRealigning;
            new_state.c_initial_sdp = c_initial_sdp;

            let mut actions = vec![
                RuleAction::UpdateLegState {
                    leg_id: c_leg_id.clone(),
                    state: LegState::Confirmed,
                    disposition: Some(call::LegDisposition::Bridged),
                },
                RuleAction::ConfirmDialog { leg_id: c_leg_id.clone() },
                RuleAction::AckLeg { leg_id: c_leg_id.clone(), body: Vec::new(), content_type: None },
            ];
            actions.extend(notify(&st, SUB_STATE_TERMINATED_NORESOURCE, 200, "OK"));
            actions.extend([
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                RuleAction::CancelTimer { id: timer_id(call::TimerType::NoAnswer, Some(&c_leg_id)) },
                RuleAction::ScheduleTimer {
                    timer_type: call::TimerType::ReferReinviteAnswer,
                    delay: TimerDelay::secs(ctx.config.refer_reinvite_answer_sec),
                    leg_id: Some(c_leg_id.clone()),
                },
                RuleAction::SendReinvite {
                    leg_id: c_leg_id.clone(),
                    body: a_sdp.to_vec(),
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
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry + overall + C no-answer" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .direction(Direction::FromB)
            .leg_states(&[LegState::Trying, LegState::Early])
            .filter(|ctx| {
                let is_fail = ctx.response().map(|r| r.status() >= 300).unwrap_or(false);
                is_fail
                    && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let mut actions = Vec::new();
            actions.extend(notify(&st, SUB_STATE_TERMINATED_NORESOURCE, resp.status(), resp.reason()));
            actions.extend([
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
            ]);
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::CancelTimer { id: timer_id(call::TimerType::NoAnswer, Some(&c_leg_id)) });
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: c_leg_id.clone(),
                    status_code: Some(resp.status() as i64),
                    reason: Some(resp.reason().to_string()),
                });
                actions.push(RuleAction::TerminateLeg {
                    leg_id: c_leg_id,
                    bye_disposition: Some(call::ByeDisposition::Rejected),
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
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry + overall" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::NoAnswer)
            .filter(|ctx| {
                let timer_leg = match ctx.event {
                    crate::event::CallEvent::Timer { leg_id, .. } => leg_id.as_deref(),
                    _ => None,
                };
                state(ctx).and_then(|s| s.c_leg_id.as_deref()).is_some()
                    && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == timer_leg
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let c_leg_id = st.c_leg_id.clone()?;
            let mut actions = Vec::new();
            actions.extend(notify(&st, SUB_STATE_TERMINATED_TIMEOUT, 408, "Request Timeout"));
            actions.extend([
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: c_leg_id.clone(),
                    status_code: None,
                    reason: Some("no_answer_timeout".to_string()),
                },
                RuleAction::DestroyLeg { leg_id: c_leg_id },
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                RuleAction::SetTransfer { state: None },
            ]);
            ok(actions)
        },
    }
}
