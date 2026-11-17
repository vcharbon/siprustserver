//! What a 481 ends (RFC 3261 §12.2.1.2): the dialog it answers in, and with
//! it the call, when that dialog still stands. A 481 to a re-INVITE, UPDATE,
//! INFO, MESSAGE, REFER or keepalive OPTIONS denies the dialog and ends the
//! call. A PRACK's (RFC 3262 §3), CANCEL's (§9.2), NOTIFY's (RFC 6665
//! §4.4.1) and BYE's (§15.1.1: the dialog ends either way) deny nothing; a
//! relayed one is `relay-non-invite-failure`'s or
//! `answer-relayed-prack-failure`'s and the rest `absorb-own-request-failure`'s.
//! A 481 in a dialog already over, or from a leg already ended, denies
//! nothing either ([`denies_a_live_dialog`]): no rule takes it.
//!
//! The 481 leg is BYEd like its peer: §12.2.1.2 ends an INVITE dialog by
//! sending a BYE, whatever the peer answered. Its final to that BYE, a 481 as
//! likely as a 200, resolves the leg (`resolve-bye-response`).

use b2bua_sdk::model::{Match, RuleAction, RuleContext, RuleDefinition};
use call::{CallModelState, CdrEventType, DialogState, LegState, TerminationCause, TimeoutKind};
use sip_message::Method;

use super::core_rules::{ok, rule};

/// Whether the current response can deny a dialog that still stands: its leg
/// is not terminated, and the dialog it names, where it names one, is early
/// or confirmed. A dialog our BYE is ending, or that has ended, has nothing
/// left for its 481 to end.
fn denies_a_live_dialog(ctx: &RuleContext) -> bool {
    ctx.source_leg().is_some_and(|leg| leg.state() != LegState::Terminated)
        && ctx
            .dialog()
            .is_none_or(|d| matches!(d.state, DialogState::Early | DialogState::Confirmed))
}

/// Whether the 481 answers a request whose 481 can deny its dialog.
fn denies_by_method(method: &Method) -> bool {
    !matches!(method, Method::Prack | Method::Cancel | Method::Notify | Method::Bye)
}

/// `handle-481`: a 481 denying a dialog that stands ends the call by its leg
/// (module doc). A 481 to the liveness probe is the probe's failure, under
/// the keepalive deadline; to anything else it is the peer's final.
pub(super) fn handle_481_rule() -> RuleDefinition {
    rule(
        "handle-481",
        &[],
        Match::response().status_code(481).call_state(CallModelState::Active).filter(|ctx| {
            ctx.response().is_some_and(|r| denies_by_method(r.cseq().method()))
                && denies_a_live_dialog(ctx)
        }),
        |ctx| {
            let src = ctx.source_leg_id.to_string();
            let probe = ctx.response().is_some_and(|r| r.cseq().method() == Method::Options);
            let cause = if probe {
                TerminationCause::Timeout(TimeoutKind::Keepalive)
            } else {
                TerminationCause::RemoteFinal
            };
            ok(vec![
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Bye,
                    leg_id: src.clone(),
                    status_code: Some(481),
                    reason: Some("Call/Transaction Does Not Exist".into()),
                },
                RuleAction::BeginTermination {
                    reason: Some("481".into()),
                    cause,
                    by_leg: Some(src),
                },
            ])
        },
    )
}
