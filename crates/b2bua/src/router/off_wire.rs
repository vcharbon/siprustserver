//! A turn the router builds itself to end a call, with no message to answer
//! (ADR-0020 X3, ADR-0052 §4): the ending's result runs through `finalize`
//! and `enforce`, whose became-terminated edge settles the CDR, the limiter
//! release and the delete; the turn's notes are drained and the result lands
//! through the turn commit. A body that reached this node already ended has
//! its edge synthesised from itself.

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::RuleAction;
use call::{Call, CallModelState, Direction};

use super::commit::commit_turn;
use super::RouterCtx;
use crate::effects::HandlerResult;
use crate::owners::Owners;
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};
use crate::rules::{ActionExecutor, RuleCall, RuleContext};
use crate::turn_scope::TurnScope;

/// What ends the call, and the call as the turn found it.
pub(super) enum Ending<'a> {
    /// A body that reached this node ended, or ending and `forced`
    /// terminal: discharged as it is, its edge read from itself as live.
    Reached { body: Call, forced: bool },
    /// The call's legs forced terminal and its CDR appended with nothing on
    /// the wire ([`crate::reaper::discharge_result`]).
    Forced { before: &'a Call },
    /// Rule actions run on the call end it.
    Actions { before: &'a Call, actions: Actions },
    /// The other owner's ended `body`, met here: actions run on the call
    /// answer what this node still holds open, their effects riding `body`.
    Met { before: &'a Call, body: Call, actions: Actions },
    /// A result a core fold built on `turn`, the turn's scope.
    Built { before: &'a Call, result: HandlerResult, turn: Box<TurnScope> },
}

/// Rule actions run on an internal event of `topic`, read by `owners`.
pub(super) struct Actions {
    pub(super) topic: &'static str,
    pub(super) list: Vec<RuleAction>,
    pub(super) owners: Owners,
}

/// Whether the ending answers a caller its INVITE leaves unanswered.
#[derive(Clone, Copy)]
pub(super) enum Caller {
    /// Her 503, under the deployment's advertisement for a minted final.
    Answer,
    /// Nothing on the wire: a body that reached this node ended, or a call
    /// with no INVITE to answer.
    Leave,
}

/// The scope of a turn the router builds itself to end a call: charged for
/// no action, since it ends the call whatever it holds (ADR-0057 §3).
pub(super) fn scope(ctx: &RouterCtx) -> TurnScope {
    TurnScope::of(ctx.id_gen.turn()).with_faults(ctx.wire_faults.snapshot()).unfueled()
}

/// End `call_ref` off the wire at `now_ms`: the ending lands at once, and
/// the returned future commits it. `label` names what the turn handled, for a
/// model-invariant violation. The caller holds the per-call lock.
pub(super) fn end<'a>(
    ctx: &'a Arc<RouterCtx>,
    call_ref: &'a str,
    ending: Ending<'_>,
    caller: Caller,
    now_ms: i64,
    label: &'static &'static str,
) -> impl Future<Output = ()> + Send + 'a {
    let result = landed(ctx, call_ref, ending, caller, now_ms);
    commit_turn(ctx, call_ref, result, now_ms, label, None)
}

/// The result `ending` lands, enforced and its turn's notes drained.
fn landed(
    ctx: &RouterCtx,
    call_ref: &str,
    ending: Ending<'_>,
    caller: Caller,
    now_ms: i64,
) -> HandlerResult {
    let (before, result, mut turn) = match ending {
        Ending::Reached { body, forced } => {
            let mut before = body.clone();
            before.state = CallModelState::Active;
            let result = if forced {
                crate::reaper::discharge_result(body, now_ms)
            } else {
                HandlerResult::new(body)
            };
            (Cow::Owned(before), result, scope(ctx))
        }
        Ending::Forced { before } => {
            let result = crate::reaper::discharge_result(before.clone(), now_ms);
            (Cow::Borrowed(before), result, scope(ctx))
        }
        Ending::Actions { before, actions } => {
            let mut turn = scope(ctx);
            let result = execute(ctx, call_ref, before, actions, &mut turn, now_ms);
            (Cow::Borrowed(before), result, turn)
        }
        Ending::Met { before, body, actions } => {
            let mut turn = scope(ctx);
            let mut result = HandlerResult::new(body);
            if !actions.list.is_empty() {
                let answered = execute(ctx, call_ref, before, actions, &mut turn, now_ms).effects;
                result.effects.extend(answered);
            }
            (Cow::Borrowed(before), result, turn)
        }
        Ending::Built { before, result, turn } => (Cow::Borrowed(before), result, *turn),
    };
    let unanswered = match caller {
        Caller::Answer => UnansweredCaller::Answer(&ctx.config.minted_final_advertisement),
        Caller::Leave => UnansweredCaller::Leave,
    };
    let result =
        enforce(&ctx.obligations, &before, finalize(result), now_ms, unanswered, &mut turn.notes);
    super::notes::drain(ctx, call_ref, &mut turn);
    result
}

/// The result of `actions` run on `call` as the turn of an internal event.
fn execute(
    ctx: &RouterCtx,
    call_ref: &str,
    call: &Call,
    actions: Actions,
    turn: &mut TurnScope,
    now_ms: i64,
) -> HandlerResult {
    let Actions { topic, list, owners } = actions;
    let event = CallEvent::InternalEvent {
        call_ref: call_ref.to_string(),
        topic: topic.into(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    };
    let rule_ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref,
        event: &event,
        source_leg_id: "",
        direction: Direction::FromA,
        now_ms,
        config: &ctx.config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut exec = ActionExecutor { config: &ctx.config, owners: &owners, turn, now_ms };
    exec.execute(&list, call, &rule_ctx)
}
