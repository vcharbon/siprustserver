//! Cross-phase guards + the overall-safety watchdog: a second REFER (491),
//! glare re-INVITEs from C or A during realign (491), the referrer B's non-BYE
//! in-dialog requests during realign (481), the referrer's 481 to a refer
//! NOTIFY (subscription ended), and the whole-transfer safety timeout
//! (rollback).

use b2bua_sdk::sm_rule;
use call::{CdrEventType, Direction};

use super::{state, timer_id, Phase, TRANSFER_MACHINE};
use crate::rules::model::{Effect, Match, RuleAction, RuleDefinition};
use crate::rules::refer_transfer::ok;

/// transfer-reject-second-refer — a second REFER while active → 491.
pub(super) fn reject_second_refer() -> RuleDefinition {
    sm_rule! {
        id: "transfer-reject-second-refer",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ReferAuthorizing, Phase::CRinging, Phase::CRealigning, Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::Respond { status: 491, label: "491 → B (second REFER pending)" },
        ],
        matcher: Match::request()
            .method("REFER")
            .direction(Direction::FromB),
        handle: |_ctx| {
            ok(vec![RuleAction::Respond {
                status: 491,
                reason: "Request Pending".to_string(),
                body: vec![],
                content_type: None,
            }])
        },
    }
}

/// transfer-c-glare-reinvite — C re-INVITEs during realigning → 491.
/// Beats CORE `reinvite-glare`/`relay-reinvite` by SERVICE_LAYER.
pub(super) fn c_glare_reinvite() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-glare-reinvite",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning, Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::Respond { status: 491, label: "491 → C (glare during realign)" },
        ],
        matcher: Match::request()
            .method("INVITE")
            .direction(Direction::FromB)
            .filter(|ctx| {
                state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
            }),
        handle: |_ctx| {
            ok(vec![RuleAction::Respond {
                status: 491,
                reason: "Request Pending".to_string(),
                body: vec![],
                content_type: None,
            }])
        },
    }
}

/// transfer-a-glare-reinvite — A re-INVITEs during realigning → 491.
/// Beats CORE `relay-reinvite` by SERVICE_LAYER.
pub(super) fn a_glare_reinvite() -> RuleDefinition {
    sm_rule! {
        id: "transfer-a-glare-reinvite",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning, Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::Respond { status: 491, label: "491 → A (glare during realign)" },
        ],
        matcher: Match::request()
            .method("INVITE")
            .direction(Direction::FromA),
        handle: |_ctx| {
            ok(vec![RuleAction::Respond {
                status: 491,
                reason: "Request Pending".to_string(),
                body: vec![],
                content_type: None,
            }])
        },
    }
}

/// transfer-overall-timeout — the overall-safety watchdog fired in any of the
/// four phases → rollback. Cancel the sub-expiry + both possible
/// `refer_reinvite_answer` ids (C's and A's), CDR timeout, begin-termination.
pub(super) fn overall_timeout() -> RuleDefinition {
    sm_rule! {
        id: "transfer-overall-timeout",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ReferAuthorizing, Phase::CRinging, Phase::CRealigning, Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry + both re-INVITE watchdogs" },
            Effect::LifecycleCommand { label: "terminate (overall-safety watchdog rollback)" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::ReferOverallSafety),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let mut actions = vec![RuleAction::CancelTimer {
                id: timer_id(call::TimerType::ReferSubscriptionExpiry, None),
            }];
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferReinviteAnswer, Some(&c_leg_id)),
                });
            }
            actions.push(RuleAction::CancelTimer {
                id: timer_id(call::TimerType::ReferReinviteAnswer, Some("a")),
            });
            actions.push(RuleAction::AddCdrEvent {
                event_type: CdrEventType::Timeout,
                leg_id: "a".to_string(),
                status_code: None,
                reason: Some("transfer-overall-timeout".to_string()),
            });
            actions.push(RuleAction::BeginTermination { reason: None });
            ok(actions)
        },
    }
}

/// transfer-notify-481 — the referrer answers a refer NOTIFY 481: the implicit
/// subscription is over (RFC 6665 §4.4.1) and no further NOTIFY leaves on it;
/// the dialog and the transfer's own outcome are untouched (RFC 3515 §2.4.6).
/// Active in every phase because the first NOTIFY is on the wire before the
/// `/call/refer` consult resolves. Claims only a NOTIFY this stack originated
/// (no pending-relay snapshot): a RELAYED NOTIFY's 481 answers the peer's
/// subscription and stays CORE `relay-non-invite-failure`'s to relay. Beats
/// CORE `absorb-own-request-failure`, the fallback for any other
/// stack-originated NOTIFY failure.
pub(super) fn referrer_notify_481() -> RuleDefinition {
    sm_rule! {
        id: "transfer-notify-481",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ReferAuthorizing, Phase::CRinging, Phase::CRealigning, Phase::ARealigning ],
        transitions: [],
        effects: [],
        matcher: Match::response()
            .method("NOTIFY")
            .status_code(481)
            .direction(Direction::FromB)
            .filter(|ctx| {
                state(ctx).map(|s| s.referrer_leg_id.as_str()) == Some(ctx.source_leg_id)
                    && !ctx.answers_relayed_request()
            }),
        handle: |ctx| {
            let mut st = state(ctx)?.clone();
            st.subscription_terminated = true;
            ok(vec![RuleAction::SetTransfer { state: Some(st) }])
        },
    }
}

/// transfer-b-in-cre-are-reject — referrer B's non-BYE in-dialog request
/// during realigning → 481 (B's signalling is "dead" until merge; its BYE
/// is still allowed through `relay-bye`).
pub(super) fn b_in_realign_reject() -> RuleDefinition {
    sm_rule! {
        id: "transfer-b-in-cre-are-reject",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning, Phase::ARealigning ],
        transitions: [],
        effects: [
            Effect::Respond { status: 481, label: "481 → B (referrer signalling dead until merge)" },
        ],
        matcher: Match::request()
            .direction(Direction::FromB)
            .filter(|ctx| {
                let method_ok = ctx
                    .request()
                    .map(|r| r.method() != "BYE")
                    .unwrap_or(false);
                method_ok
                    && state(ctx).map(|s| s.referrer_leg_id.as_str()) == Some(ctx.source_leg_id)
            }),
        handle: |_ctx| {
            ok(vec![RuleAction::Respond {
                status: 481,
                reason: "Call/Transaction Does Not Exist".to_string(),
                body: vec![],
                content_type: None,
            }])
        },
    }
}
