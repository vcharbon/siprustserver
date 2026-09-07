//! CRealigning / ARealigning phases: re-INVITE C onto A's SDP, then re-INVITE
//! A onto C's active answer, and merge A↔C on success. Each realign step has a
//! 200 / fail / timeout triple; failures roll the whole call back
//! (begin-termination BYEs A, B and C).

use b2bua_sdk::sm_rule;
use call::{CdrEventType, Direction, LegState, TransferPhase};
use sip_message::Method;

use super::{state, timer_id, Phase, TRANSFER_MACHINE};
use crate::rules::model::{Effect, Match, RuleAction, RuleDefinition, TimerDelay};
use crate::rules::refer_transfer::ok;
use crate::rules::Terminal;

/// transfer-c-realign-200 — C answers the c-realign re-INVITE (200).
/// Distinguished from `transfer-c-200-initial` by `legState: confirmed`
/// (the initial INVITE answered trying/early).
pub(super) fn c_realign_200() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-realign-200",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning ],
        transitions: [ Phase::CRealigning => Phase::ARealigning ],
        effects: [
            Effect::Originate { method: Method::Ack, label: "ACK → C (c-realign answered)" },
            Effect::Originate { method: Method::Invite, label: "re-INVITE → A (a-realign, C's active SDP)" },
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "cancel C / arm A re-INVITE watchdog" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(2)
            .direction(Direction::FromB)
            .leg_states(&[LegState::Confirmed])
            .filter(|ctx| {
                state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let c_leg_id = st.c_leg_id.clone()?;

            // Offer A the active SDP C just answered on the c-realign
            // re-INVITE (sendrecv, C's real port/codec) so A enables its
            // send path. C's *initial* held answer would leave A inactive →
            // one-way audio.
            let c_realign_sdp = resp.body().clone();

            let mut new_state = st.clone();
            new_state.phase = TransferPhase::ARealigning;

            ok(vec![
                RuleAction::AckLeg { leg_id: c_leg_id.clone(), body: Vec::new(), content_type: None },
                RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferReinviteAnswer, Some(&c_leg_id)),
                },
                RuleAction::ScheduleTimer {
                    timer_type: call::TimerType::ReferReinviteAnswer,
                    delay: TimerDelay::secs(ctx.config.refer_reinvite_answer_sec),
                    leg_id: Some("a".to_string()),
                },
                RuleAction::SendReinvite {
                    leg_id: "a".to_string(),
                    body: c_realign_sdp.to_vec(),
                    add_headers: vec![],
                },
                RuleAction::SetTransfer { state: Some(new_state) },
            ])
        },
    }
}

/// transfer-c-realign-fail — C rejects the c-realign re-INVITE → rollback.
/// `legState: confirmed` distinguishes this from `transfer-c-fail-initial`.
/// begin-termination BYEs all three confirmed legs (A, B, C). The slice is
/// NOT cleared — the call termination path drops it.
pub(super) fn c_realign_fail() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-realign-fail",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning ],
        transitions: [],
        effects: [
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel re-INVITE watchdog + overall" },
            Effect::LifecycleCommand { label: "terminate (c-realign rollback — BYE A/B/C)" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .direction(Direction::FromB)
            .leg_states(&[LegState::Confirmed])
            .filter(|ctx| {
                let is_fail = ctx.response().map(|r| r.status() >= 300).unwrap_or(false);
                is_fail
                    && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
            }),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let resp = ctx.response()?;
            let mut actions = vec![];
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferReinviteAnswer, Some(&c_leg_id)),
                });
            }
            actions.push(RuleAction::CancelTimer {
                id: timer_id(call::TimerType::ReferOverallSafety, None),
            });
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: c_leg_id,
                    status_code: Some(resp.status() as i64),
                    reason: Some("transfer-rollback-c-realign".to_string()),
                });
            }
            actions.push(RuleAction::BeginTermination { reason: None });
            ok(actions)
        },
    }
}

/// transfer-c-realign-timeout — `refer_reinvite_answer` fired while
/// c-realigning (C never answered the re-INVITE) → rollback. Shares the
/// timer type with `transfer-a-realign-timeout`; the active-state gate
/// keeps them mutually exclusive.
pub(super) fn c_realign_timeout() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-realign-timeout",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning ],
        transitions: [],
        effects: [
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
            Effect::LifecycleCommand { label: "terminate (c-realign timeout rollback)" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::ReferReinviteAnswer),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let mut actions = vec![RuleAction::CancelTimer {
                id: timer_id(call::TimerType::ReferOverallSafety, None),
            }];
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: c_leg_id,
                    status_code: None,
                    reason: Some("transfer-rollback-c-realign".to_string()),
                });
            }
            actions.push(RuleAction::BeginTermination { reason: None });
            ok(actions)
        },
    }
}

/// transfer-a-realign-200 — A answers the a-realign re-INVITE (200) →
/// merge(a, c). The transfer is complete: ACK A, cancel A's
/// `refer_reinvite_answer` + the overall-safety timer, `merge(a, cLegId)`
/// (A↔C now bridged), CDR answer "transfer-completed", and clear the slice.
/// B is left an orphan confirmed leg — a subsequent A BYE → begin-termination
/// BYEs both B and C.
pub(super) fn a_realign_200() -> RuleDefinition {
    sm_rule! {
        id: "transfer-a-realign-200",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ARealigning ],
        transitions: [ Phase::ARealigning => Terminal ],
        effects: [
            Effect::Originate { method: Method::Ack, label: "ACK → A (a-realign answered)" },
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "cancel A re-INVITE watchdog + overall" },
            Effect::LifecycleCommand { label: "merge A↔C (transfer complete)" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(2)
            .direction(Direction::FromA)
            .filter(|ctx| ctx.source_leg_id == "a"),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let c_leg_id = st.c_leg_id.clone()?;
            ok(vec![
                RuleAction::AckLeg { leg_id: "a".to_string(), body: Vec::new(), content_type: None },
                RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferReinviteAnswer, Some("a")),
                },
                RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferOverallSafety, None),
                },
                RuleAction::Merge {
                    leg_a: "a".to_string(),
                    leg_b: c_leg_id,
                },
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Answer,
                    leg_id: "a".to_string(),
                    status_code: Some(200),
                    reason: Some("transfer-completed".to_string()),
                },
                RuleAction::SetTransfer { state: None },
            ])
        },
    }
}

/// transfer-a-realign-fail — A rejects the a-realign re-INVITE → rollback.
/// begin-termination BYEs all three confirmed legs (A, B, C); the slice is
/// dropped as the call terminates.
pub(super) fn a_realign_fail() -> RuleDefinition {
    sm_rule! {
        id: "transfer-a-realign-fail",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "cancel A re-INVITE watchdog + overall" },
            Effect::LifecycleCommand { label: "terminate (a-realign rollback — BYE A/B/C)" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .direction(Direction::FromA)
            .filter(|ctx| {
                let is_fail = ctx.response().map(|r| r.status() >= 300).unwrap_or(false);
                is_fail && ctx.source_leg_id == "a"
            }),
        handle: |ctx| {
            let resp = ctx.response()?;
            ok(vec![
                RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferReinviteAnswer, Some("a")),
                },
                RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferOverallSafety, None),
                },
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: "a".to_string(),
                    status_code: Some(resp.status() as i64),
                    reason: Some("transfer-rollback-a-realign".to_string()),
                },
                RuleAction::BeginTermination { reason: None },
            ])
        },
    }
}

/// transfer-a-realign-timeout — `refer_reinvite_answer` fired while
/// a-realigning (A never answered) → rollback. Shares the timer type with
/// `transfer-c-realign-timeout`; the active-state gate + the fired timer's
/// leg=="a" keep them mutually exclusive.
pub(super) fn a_realign_timeout() -> RuleDefinition {
    sm_rule! {
        id: "transfer-a-realign-timeout",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
            Effect::LifecycleCommand { label: "terminate (a-realign timeout rollback)" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::ReferReinviteAnswer)
            .filter(|ctx| {
                let timer_leg = match ctx.event {
                    crate::event::CallEvent::Timer { leg_id, .. } => leg_id.as_deref(),
                    _ => None,
                };
                timer_leg == Some("a")
            }),
        handle: |_ctx| {
            ok(vec![
                RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferOverallSafety, None),
                },
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: "a".to_string(),
                    status_code: None,
                    reason: Some("transfer-rollback-a-realign".to_string()),
                },
                RuleAction::BeginTermination { reason: None },
            ])
        },
    }
}
