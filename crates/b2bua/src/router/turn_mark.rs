//! A turn's marks at the turn-point seam (ADR-0055 §13): what the turn was
//! taken on and what it started, reported at each sub-point it reaches.

use sip_message::SipRequest;

use super::RouterCtx;
use crate::effects::{FireAndForgetEffect, HandlerResult};
use crate::turn_probe::{Released, TurnMark, TurnPoint};

/// What a turn was taken on and what it started, as its turn marks report.
#[derive(Clone, Copy, Default)]
pub(super) struct TurnKind {
    on_request: bool,
    on_answer: bool,
    called_out: bool,
    resent: bool,
    reasked: bool,
}

impl TurnKind {
    /// A turn taken on a SIP request that started nothing off the node.
    pub(super) fn on_request() -> Self {
        Self { on_request: true, ..Self::default() }
    }

    /// The kind of the turn that landed `result`, taken on `received` when it
    /// handled a SIP request.
    pub(super) fn of(result: &HandlerResult, received: Option<&SipRequest>) -> Self {
        let called_out = result
            .effects
            .fire_and_forget
            .iter()
            .any(|e| !matches!(e, FireAndForgetEffect::Reenter(_)));
        let on_answer = result.effects.took_answer;
        let (resent, reasked) = (result.effects.resent, result.effects.reasked);
        Self { on_request: received.is_some(), on_answer, called_out, resent, reasked }
    }
}

/// Report the turn of `call_ref` at `point` to the turn-point seam, at the
/// write counters `version` the turn left.
pub(super) async fn reach(
    ctx: &RouterCtx,
    call_ref: &str,
    point: TurnPoint,
    version: (i64, i64),
    wrote: bool,
    kind: TurnKind,
    released: Option<Released>,
) {
    ctx.turn_probe
        .at(|| TurnMark {
            node: &ctx.config.self_ordinal,
            call_ref,
            point,
            version,
            wrote,
            removed: false,
            on_request: kind.on_request,
            on_answer: kind.on_answer,
            called_out: kind.called_out,
            resent: kind.resent,
            reasked: kind.reasked,
            released,
            backed: point == TurnPoint::Logged && ctx.state.hold_target(call_ref).is_some(),
        })
        .await;
}
