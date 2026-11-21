//! CRealigning / ARealigning phases: re-INVITE C onto A's SDP, then re-INVITE
//! A onto C's active answer, and merge A↔C on success. Each realign step has a
//! 200 / fail / timeout triple; failures roll the whole call back
//! (begin-termination BYEs A, B and C).

use b2bua_sdk::sm_rule;
use call::{CdrEventType, Direction, LegState, TerminationCause, TimeoutKind, TransferPhase};
use sip_message::Method;

use sip_message::SipResponse;

use super::{state, Phase, TRANSFER_MACHINE};
use crate::rules::refer_transfer::ok;
use crate::rules::{relay, Terminal};
use b2bua_sdk::model::{Body, Effect, Match, RuleAction, RuleContext, RuleDefinition, TimerDelay};
use call::TimerKey;

/// The longest the ACK of C's offering c-realign 2xx waits for A's answer:
/// four T1 inside C's 64·T1 give-up on that 2xx (RFC 3261 §13.3.1.4), so the
/// rollback's ACK reaches C before C gives the 2xx up.
const HELD_ACK_BOUND_MS: i64 = (64 - 4) * sip_retransmit::timers::T1 as i64;

/// transfer-c-realign-200 — C answers the c-realign re-INVITE (200).
/// Distinguished from `transfer-c-200-initial` by `legState: confirmed`
/// (the initial INVITE answered trying/early). A copy of the 2xx to C's
/// first INVITE (RFC 3261 §13.3.1.4) answers nothing pending
/// ([`RuleContext::answers_last_sent_invite`]); it is `re-ack-2xx-ahead`'s.
/// Where the re-INVITE carried no offer and C offers in its 2xx, C's ACK
/// waits for A's answer to the a-realign re-INVITE, which carries C's offer
/// (RFC 3725 §4.1, flow I), and A's watchdog runs at most
/// [`HELD_ACK_BOUND_MS`]; otherwise C is ACKed bare now.
///
/// [`RuleContext::answers_last_sent_invite`]: b2bua_sdk::model::RuleContext::answers_last_sent_invite
pub(super) fn c_realign_200() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-realign-200",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning ],
        transitions: [ Phase::CRealigning => Phase::ARealigning ],
        effects: [
            Effect::Originate { method: Method::Ack, label: "ACK → C (c-realign answered; held for A's answer when C offers)" },
            Effect::Originate { method: Method::Invite, label: "re-INVITE → A (a-realign, C's active SDP)" },
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, scope: call::TimerScopeKind::Leg, label: "cancel C / arm A re-INVITE watchdog" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(2)
            .direction(Direction::FromB)
            .leg_states(&[LegState::Confirmed])
            .filter(|ctx| {
                state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                    && ctx.answers_last_sent_invite()
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
            let watchdog = TimerDelay::secs(ctx.config.refer_reinvite_answer_sec);
            let watchdog = if offers_to_its_ack(ctx, &c_leg_id) {
                TimerDelay::millis(watchdog.as_millis().min(HELD_ACK_BOUND_MS))
            } else {
                watchdog
            };

            let mut new_state = st.clone();
            new_state.phase = TransferPhase::ARealigning;

            ok(vec![
                RuleAction::AckLegAnswering { leg_id: c_leg_id.clone(), answer: None },
                RuleAction::cancel(TimerKey::leg(call::TimerType::ReferReinviteAnswer, &c_leg_id)),
                RuleAction::arm(TimerKey::leg(call::TimerType::ReferReinviteAnswer, "a"), watchdog),
                RuleAction::SendReinvite {
                    leg_id: "a".to_string(),
                    body: Some(
                        Body::from_leg(c_realign_sdp.to_vec(), c_leg_id.clone())
                            .described_by(resp.headers()),
                    ),
                    add_headers: vec![],
                },
                RuleAction::SetTransfer { state: Some(new_state) },
            ])
        },
    }
}

/// Whether `ctx`'s 2xx, C's to the c-realign re-INVITE on `c_leg_id`, makes
/// an offer its ACK owes the answer to: it carries a session description and
/// the re-INVITE carried none (RFC 3264 §4).
fn offers_to_its_ack(ctx: &RuleContext, c_leg_id: &str) -> bool {
    let described = ctx.response().is_some_and(|r| r.sdp().is_some());
    let offerless = ctx.call.b_legs().iter().find(|l| l.leg_id == c_leg_id).is_some_and(|l| {
        l.dialogs.confirmed().is_some_and(|d| !relay::acked_invite_carries_offer(l, d))
    });
    described && offerless
}

/// The ACK of C's c-realign 2xx, held for A's answer
/// ([`transfer-c-realign-200`](c_realign_200)), carrying `answer`, A's 2xx
/// to the a-realign re-INVITE: nothing where C's ACK holds for no answer (it
/// left bare).
fn ack_c_with(ctx: &RuleContext, c_leg_id: &str, answer: &SipResponse) -> Option<RuleAction> {
    let c_leg = ctx.call.b_legs().iter().find(|l| l.leg_id == c_leg_id)?;
    relay::offer_owed_in_ack(c_leg)?;
    let answer = Body::from_leg(answer.body().to_vec(), "a").described_by(answer.headers());
    Some(RuleAction::AckLegAnswering { leg_id: c_leg_id.to_string(), answer: Some(answer) })
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
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, scope: call::TimerScopeKind::Leg, label: "cancel the re-INVITE watchdog" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall" },
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
                actions.push(RuleAction::cancel(TimerKey::leg(
                    call::TimerType::ReferReinviteAnswer,
                    &c_leg_id,
                )));
            }
            actions.push(RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)));
            if let Some(c_leg_id) = st.c_leg_id.clone() {
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: c_leg_id,
                    status_code: Some(resp.status() as i64),
                    reason: Some("transfer-rollback-c-realign".to_string()),
                });
            }
            actions.push(RuleAction::BeginTermination {
                reason: None,
                cause: TerminationCause::RemoteFinal,
                by_leg: st.c_leg_id.clone(),
            });
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
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall-safety" },
            Effect::LifecycleCommand { label: "terminate (c-realign timeout rollback)" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::ReferReinviteAnswer),
        handle: |ctx| ok(c_realign_rollback(ctx, None)?),
    }
}

/// transfer-c-realign-txn-timeout — the c-realign re-INVITE's client
/// transaction gave up (Timer B, or the INVITE bound after a provisional)
/// → the same rollback as its watchdog. C does not carry the call before the
/// merge, so the default would end C's dialog alone; the transfer machine
/// owns that failure.
pub(super) fn c_realign_txn_timeout() -> RuleDefinition {
    sm_rule! {
        id: "transfer-c-realign-txn-timeout",
        machine: TRANSFER_MACHINE,
        active: [ Phase::CRealigning ],
        transitions: [],
        effects: [
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, scope: call::TimerScopeKind::Leg, label: "cancel the re-INVITE watchdog" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall-safety" },
            Effect::LifecycleCommand { label: "terminate (c-realign timeout rollback)" },
        ],
        matcher: Match::timeout().filter(|ctx| {
            ctx.timeout_method() == Some("INVITE")
                && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
        }),
        handle: |ctx| {
            let watchdog = TimerKey::leg(call::TimerType::ReferReinviteAnswer, ctx.source_leg_id);
            ok(c_realign_rollback(ctx, Some(watchdog))?)
        },
    }
}

/// The c-realign rollback: cancel `watchdog` when it still runs and the
/// overall-safety timer, record C's timeout and end the call (A, B and C are
/// BYE'd).
fn c_realign_rollback(
    ctx: &b2bua_sdk::model::RuleContext,
    watchdog: Option<TimerKey>,
) -> Option<Vec<RuleAction>> {
    let st = state(ctx)?.clone();
    let mut actions: Vec<RuleAction> = watchdog.into_iter().map(RuleAction::cancel).collect();
    actions.push(RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)));
    if let Some(c_leg_id) = st.c_leg_id.clone() {
        actions.push(RuleAction::AddCdrEvent {
            event_type: CdrEventType::Timeout,
            leg_id: c_leg_id,
            status_code: None,
            reason: Some("transfer-rollback-c-realign".to_string()),
        });
    }
    actions.push(RuleAction::BeginTermination {
        reason: None,
        cause: TerminationCause::Timeout(TimeoutKind::NoAnswer),
        by_leg: st.c_leg_id.clone(),
    });
    Some(actions)
}

/// transfer-a-realign-200 — A answers the a-realign re-INVITE (200) →
/// merge(a, c). The transfer is complete: ACK A, cancel A's
/// `refer_reinvite_answer` + the overall-safety timer, `merge(a, cLegId)`
/// (A↔C now bridged), release the referrer B when it has not hung up (BYE;
/// a BYE of B's crossing it is `resolve-cross-bye`'s), CDR answer
/// "transfer-completed", and clear the slice. A copy of the 2xx to an earlier
/// INVITE on A's dialog answers nothing pending; it is `re-ack-2xx-ahead`'s.
/// C's ACK held for A's answer leaves after A's, carrying it (RFC 3725 §4.1).
pub(super) fn a_realign_200() -> RuleDefinition {
    sm_rule! {
        id: "transfer-a-realign-200",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ARealigning ],
        transitions: [ Phase::ARealigning => Terminal ],
        effects: [
            Effect::Originate { method: Method::Ack, label: "ACK → A (a-realign answered)" },
            Effect::Originate { method: Method::Ack, label: "ACK → C carrying A's answer (when held)" },
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, scope: call::TimerScopeKind::Leg, label: "cancel A re-INVITE watchdog" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall" },
            Effect::LifecycleCommand { label: "merge A↔C (transfer complete)" },
            Effect::Originate { method: Method::Bye, label: "BYE → B (release the referrer)" },
        ],
        matcher: Match::response()
            .method("INVITE")
            .status_class(2)
            .direction(Direction::FromA)
            .filter(|ctx| ctx.source_leg_id == "a" && ctx.answers_last_sent_invite()),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let c_leg_id = st.c_leg_id.clone()?;
            let resp = ctx.response()?;
            let acks = std::iter::once(RuleAction::AckLeg { leg_id: "a".to_string(), body: None })
                .chain(ack_c_with(ctx, &c_leg_id, resp));
            ok(acks.chain([
                RuleAction::cancel(TimerKey::leg(call::TimerType::ReferReinviteAnswer, "a")),
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)),
                RuleAction::Merge {
                    leg_a: "a".to_string(),
                    leg_b: c_leg_id,
                },
                RuleAction::DestroyLeg { leg_id: st.referrer_leg_id.clone(), headers: vec![] },
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Answer,
                    leg_id: "a".to_string(),
                    status_code: Some(200),
                    reason: Some("transfer-completed".to_string()),
                },
                RuleAction::SetTransfer { state: None },
            ])
            .collect())
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
            Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, scope: call::TimerScopeKind::Leg, label: "cancel A re-INVITE watchdog" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall" },
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
                RuleAction::cancel(TimerKey::leg(call::TimerType::ReferReinviteAnswer, "a")),
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)),
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: "a".to_string(),
                    status_code: Some(resp.status() as i64),
                    reason: Some("transfer-rollback-a-realign".to_string()),
                },
                RuleAction::BeginTermination {
                    reason: None,
                    cause: TerminationCause::RemoteFinal,
                    by_leg: Some("a".to_string()),
                },
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
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, scope: call::TimerScopeKind::Call, label: "cancel overall-safety" },
            Effect::LifecycleCommand { label: "terminate (a-realign timeout rollback)" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::ReferReinviteAnswer)
            .filter(|ctx| {
                let timer_leg = ctx.timer_key().and_then(TimerKey::leg_id);
                timer_leg == Some("a")
            }),
        handle: |_ctx| {
            ok(vec![
                RuleAction::cancel(TimerKey::call(call::TimerType::ReferOverallSafety)),
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: "a".to_string(),
                    status_code: None,
                    reason: Some("transfer-rollback-a-realign".to_string()),
                },
                RuleAction::BeginTermination {
                    reason: None,
                    cause: TerminationCause::Timeout(TimeoutKind::NoAnswer),
                    by_leg: Some("a".to_string()),
                },
            ])
        },
    }
}
