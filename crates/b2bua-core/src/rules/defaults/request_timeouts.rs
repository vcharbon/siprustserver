//! `handle-timeout`, the default reading of a client transaction's timeout. A
//! timeout terminates what the request belongs to on its own leg (RFC 3261
//! §12.2.1.2, §14.1; RFC 5057 §5): the transaction, the usage or the dialog.
//! It ends the whole call only where that dialog carries the call
//! ([`call::helpers::carries_call`]); on any other leg the dialog alone is
//! terminated, and the service that owns the leg hears it end. The table,
//! for a request this stack sent itself or relayed (ADR-0068):
//!
//! | request | carrying leg | other leg | leg going away |
//! | --- | --- | --- | --- |
//! | PRACK | transaction only | transaction only | own: the leg's end; relayed: nothing |
//! | NOTIFY, SUBSCRIBE, REFER | its subscription usage only | the same | own: the leg's end; relayed: nothing |
//! | keepalive OPTIONS | the call, under the keepalive deadline | the leg | the leg's end |
//! | any other non-INVITE | the call | the leg | own: the leg's end; relayed: nothing |
//! | re-INVITE, own or relayed | the call | the leg | the leg's end |
//! | a pending leg's dialling INVITE | the setup's failure reading | the same | the leg's end |
//!
//! A relayed request's originator is sent no 408 (RFC 4320 §4.2). A service
//! rule outranks this default for a request of its own
//! ([`b2bua_sdk::model::RuleContext::timed_out_record`] names the rule that
//! sent it); keeping the call after a carrying leg's re-INVITE timeout is a
//! departure from §14.1 that service states.

use b2bua_sdk::model::{LegEnd, Match, RuleAction, RuleContext, RuleDefinition};
use b2bua_sdk::provisional::originator_final_sent;
use call::{
    ByeDisposition, CdrEventType, LegDisposition, LegState, TerminationCause, TimeoutKind,
    TxnTimeoutKind,
};
use sip_message::Method;

use super::core_rules::{ok, rule};
use super::keepalive_failure::keepalive_failure;
use super::straggler::straggler_release_timeout;

/// What the timeout of a request ends by its method, its leg still up (RFC
/// 5057 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// Nothing beyond the transaction: a PRACK's acknowledgement alone.
    Transaction,
    /// The usage the request belongs to or opens: a subscription's (NOTIFY,
    /// SUBSCRIBE, REFER).
    Usage,
    /// The dialog the request rode.
    Dialog,
}

/// The scope of a timed-out request of `method`.
fn scope(method: &str) -> Scope {
    match Method::from_wire(method) {
        Method::Prack => Scope::Transaction,
        Method::Notify | Method::Subscribe | Method::Refer => Scope::Usage,
        _ => Scope::Dialog,
    }
}

/// The `handle-timeout` rule (module doc). A teardown rule: on a terminating
/// call the dead transaction still resolves its leg.
pub(super) fn handle_timeout() -> RuleDefinition {
    rule("handle-timeout", &[], Match::timeout(), |ctx| ok(timeout_actions(ctx)))
        .runs_while_terminating()
}

/// The actions of `handle-timeout` (module doc). The BYE that released a
/// fork straggler times out the release's alone (§15.1.1): the winning dialog
/// on the leg never reads it.
fn timeout_actions(ctx: &RuleContext) -> Vec<RuleAction> {
    if straggler_release_timeout(ctx) {
        return vec![];
    }
    if let Some(record) = ctx.timed_out_record() {
        return record_timeout_actions(ctx, record);
    }
    if let Some(leg) = ctx.source_leg() {
        if call::helpers::leg_is_going_away(ctx.call.state(), leg) {
            return going_away_timeout(ctx, leg);
        }
    }
    if let Some(actions) = pending_leg_invite_timeout(ctx) {
        return actions;
    }
    dialog_timeout(ctx)
}

/// What the timeout of the request `record` settled means (module doc,
/// ADR-0055 §2): read by its method, the same for a request this stack sent
/// and one it relayed. A keepalive probe's ends the call under the keepalive
/// deadline (`super::keepalive_failure`).
pub(super) fn record_timeout_actions(
    ctx: &RuleContext,
    record: &call::RequestRecord,
) -> Vec<RuleAction> {
    let scope = scope(&record.method);
    if scope == Scope::Transaction {
        return vec![];
    }
    if let Some(leg) = ctx.source_leg() {
        if call::helpers::leg_is_going_away(ctx.call.state(), leg) {
            return if record.is_relay() { vec![] } else { going_away_timeout(ctx, leg) };
        }
    }
    match scope {
        Scope::Transaction | Scope::Usage => vec![],
        Scope::Dialog if !ctx.call.carries_call(ctx.source_leg_id) => end_the_leg(ctx),
        Scope::Dialog if record.is_keepalive() => keepalive_failure(ctx, None, "keepalive timeout"),
        Scope::Dialog => dialog_timeout(ctx),
    }
}

/// A dialog that did not answer: the call ends where the dialog carries it,
/// the leg alone elsewhere.
fn dialog_timeout(ctx: &RuleContext) -> Vec<RuleAction> {
    if !ctx.call.carries_call(ctx.source_leg_id) {
        return end_the_leg(ctx);
    }
    vec![RuleAction::BeginTermination {
        reason: Some("timeout".into()),
        cause: TerminationCause::Timeout(TimeoutKind::Transaction),
        by_leg: Some(ctx.source_leg_id.to_string()),
    }]
}

/// The leg that did not answer, carrying nothing of the call, is ended as
/// its state asks (BYE, or CANCEL while early); the call stands.
fn end_the_leg(ctx: &RuleContext) -> Vec<RuleAction> {
    vec![RuleAction::DestroyLeg { leg_id: ctx.source_leg_id.to_string(), headers: vec![] }]
}

/// A pending b-leg's dialling INVITE gave up (Timer B, or the INVITE bound
/// after a provisional): the setup's failure reading. On a failover-capable
/// call the decision backend gets its shot (the dead-gateway reroute): the
/// timeout recorded, the failed leg destroyed (its CANCEL meets the
/// transaction still open: sent if the callee answered a provisional, held
/// for one otherwise, §9.1), and `call-failure-result` drives the outcome.
/// Without one the caller is answered in this turn, before the CANCELed leg
/// resolves (§9.1 lets that wait 64·T1), and the call ends. The consult
/// states which timeout fired, so the decision service routes a dead hop
/// (`response`: nothing answered) apart from a callee that rang and went
/// silent (`transaction`), and an empty relayable image: a blackholed hop drew
/// no final, so an earlier attempt's headers cannot answer it. `None` for any
/// other timeout.
fn pending_leg_invite_timeout(ctx: &RuleContext) -> Option<Vec<RuleAction>> {
    let timed_out_invite = ctx.timeout_method().is_some_and(|m| m == "INVITE");
    let pending_b_leg = ctx.source_leg().is_some_and(|l| {
        l.leg_id != ctx.call.a_leg().leg_id
            && matches!(l.state(), LegState::Trying | LegState::Early)
    });
    if !(timed_out_invite && pending_b_leg) {
        return None;
    }
    let response_timeout = ctx.timeout_kind() == Some(TxnTimeoutKind::Response);
    let Some(cbctx) = ctx.call.callback_context() else {
        let mut actions = Vec::new();
        if !originator_final_sent(&ctx.call) {
            actions.push(super::failover_rules::timeout_final(response_timeout));
        }
        actions.push(RuleAction::BeginTermination {
            reason: Some("timeout".into()),
            cause: TerminationCause::Timeout(TimeoutKind::Transaction),
            by_leg: Some(ctx.source_leg_id.to_string()),
        });
        return Some(actions);
    };
    let leg = ctx.source_leg_id.to_string();
    let timeout_kind = if response_timeout { "response" } else { "transaction" };
    Some(vec![
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Timeout,
            leg_id: leg.clone(),
            status_code: None,
            reason: Some("transaction_timeout".into()),
        },
        RuleAction::DestroyLeg { leg_id: leg.clone(), headers: vec![] },
        RuleAction::MergeCallExt {
            ext: crate::rules::relay::failure_headers_ext(
                None,
                ctx.call.a_leg_invite(),
                ctx.config,
            ),
        },
        RuleAction::FailureAsyncHttp {
            request: serde_json::json!({
                "callback_context": cbctx,
                "origin": "transaction_timeout",
                "timeout_kind": timeout_kind,
                "failed_leg_id": leg,
            }),
        },
    ])
}

/// A transaction timing out on `leg`, a leg already going away, makes no
/// forward progress: what the leg was awaiting names its end. No
/// `/call/failure` consult and no `BeginTermination`, which would slide the
/// terminating backstop of a call already tearing down. A leg awaiting its
/// BYE's answer is resolved by that BYE's own timeout only: an earlier
/// request's dead transaction (the liveness OPTIONS that opened the teardown,
/// Timer F still running) says nothing about the BYE, which may yet be
/// answered. A leg already ended (terminal disposition, no CANCEL in flight)
/// keeps its disposition when a request sent after its dialog ended times out
/// (a NOTIFY on a surviving subscription usage, RFC 5057 §5.2); the requests
/// it still holds are settled by the leg's re-termination. Anything else is
/// abandoned.
pub(super) fn going_away_timeout(ctx: &RuleContext, leg: &call::Leg) -> Vec<RuleAction> {
    let leg_id = ctx.source_leg_id.to_string();
    if leg.disposition() != LegDisposition::Cancelling
        && matches!(leg.end(), Some(b) if b.is_terminal())
    {
        return vec![RuleAction::TerminateLeg { leg_id, end: LegEnd::Settle }];
    }
    let timed_out_bye = ctx.timeout_method().is_some_and(|m| m == "BYE");
    let end = match leg.end() {
        Some(ByeDisposition::ByeSent) if timed_out_bye => LegEnd::OurByeTimedOut,
        Some(ByeDisposition::ByeSent) => return vec![],
        _ => LegEnd::Abandon,
    };
    vec![RuleAction::TerminateLeg { leg_id, end }]
}
