//! The rule fixtures (test support) for what the turn driver
//! ([`crate::turn_driver`]) does not reach: the one context builder a
//! selection or matcher test reads a call through, and the one executor
//! builder an action-primitive test runs its actions on.

use b2bua_sdk::event::CallEvent;
use call::helpers::Resolved;
use call::{Call, Direction, Obligation};

use crate::config::B2buaConfig;
use crate::effects::HandlerResult;
use crate::owners::Owners;
use crate::rules::{ActionExecutor, RuleAction, RuleCall, RuleContext};
use crate::turn_scope::TurnScope;

/// The context of `event` arriving on `leg` of `call` under `config`: at
/// instant 0, nothing discharged, its message resolved to nothing.
pub fn context<'a>(
    call: &'a Call,
    event: &'a CallEvent,
    leg: &'a str,
    config: &'a B2buaConfig,
) -> RuleContext<'a> {
    RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event,
        source_leg_id: leg,
        direction: if leg == call.a_leg.leg_id { Direction::FromA } else { Direction::FromB },
        now_ms: 0,
        config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    }
}

/// A context as a test varies it.
pub trait Varied<'a> {
    /// The context at instant `now_ms`.
    fn at(self, now_ms: i64) -> Self;
    /// The context, its message resolved as `resolved`.
    fn resolving(self, resolved: &'a Resolved) -> Self;
    /// The context, its ACK or PRACK having discharged `obligation`.
    fn discharging(self, obligation: &'a Obligation) -> Self;
}

impl<'a> Varied<'a> for RuleContext<'a> {
    fn at(mut self, now_ms: i64) -> Self {
        self.now_ms = now_ms;
        self
    }

    fn resolving(mut self, resolved: &'a Resolved) -> Self {
        self.resolved = resolved;
        self
    }

    fn discharging(mut self, obligation: &'a Obligation) -> Self {
        self.discharged = Some(obligation);
        self
    }
}

/// The executor of the turn `turn` under `config`, at instant 0, with no
/// owner stated.
pub fn executor<'a>(config: &'a B2buaConfig, turn: &'a mut TurnScope) -> ActionExecutor<'a> {
    ActionExecutor { config, owners: Owners::none(), turn, now_ms: 0 }
}

/// `actions` executed on `call` in the turn `turn_seq` of the fixed test
/// key, read through `ctx`, as the context's clock reads.
pub fn execute(
    ctx: &RuleContext<'_>,
    call: &Call,
    turn_seq: u64,
    actions: &[RuleAction],
) -> HandlerResult {
    let mut turn = crate::test_call::turn(turn_seq);
    let mut exec = executor(ctx.config, &mut turn);
    exec.now_ms = ctx.now_ms;
    exec.execute(actions, call, ctx)
}
