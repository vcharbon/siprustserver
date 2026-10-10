//! The final an in-dialog request draws when the relay it was given to has no
//! dialog to carry it ([`Unrelayed`]): a relay never leaves its sender to run
//! out Timer F (RFC 3261 §8.2, §17.2.2).

use call::Call;
use sip_message::header::HeaderName;
use sip_message::{Method, SipHeader, SipStr};

use crate::effects::HandlerEffects;
use b2bua_sdk::model::RuleContext;

use super::ActionExecutor;

/// The `Retry-After` a request no dialog carries yet states: one second.
const NOT_NOW_RETRY_AFTER_SEC: u32 = 1;

/// Why a relayed request has no dialog to ride.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unrelayed {
    /// No dialog carries it now while its own dialog lives on: no peer leg,
    /// a peer leg with no dialog yet or with several and none named, or a
    /// peer dialog its party was never shown or no longer live. 500 with
    /// `Retry-After` (RFC 3261 §14.2, RFC 3311 §5.2): the sender may retry,
    /// and its dialog stays — a 481 would end it (§12.2.1.2).
    NotNow,
    /// The request comes from a leg its service owns (ADR-0014), and no rule
    /// of that service took it: 403 (§21.4.4).
    ServiceOwned,
}

impl ActionExecutor<'_> {
    /// Answer the turn's request for `why`; an ACK is never answered
    /// (§17.1.1.3), and a response has no answer.
    pub(super) fn refuse_unrelayed(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        why: Unrelayed,
    ) {
        let Some(req) = ctx.request() else { return };
        if *req.method() == Method::Ack {
            return;
        }
        match why {
            // A method no relay ever carries takes its permanent final, not
            // a retry that cannot succeed.
            Unrelayed::NotNow if super::select::in_dialog_method(req.method()).is_none() => {
                self.refuse_unrelayable(call, fx, ctx, req);
            }
            Unrelayed::NotNow => {
                let retry_after = SipHeader {
                    name: SipStr::owned(HeaderName::RetryAfter.as_wire_str()),
                    value: SipStr::owned(&NOT_NOW_RETRY_AFTER_SEC.to_string()),
                };
                let reason = "Server Internal Error";
                self.respond(call, fx, ctx, 500, reason, &[], None, vec![retry_after]);
            }
            Unrelayed::ServiceOwned => {
                self.respond(call, fx, ctx, 403, "Forbidden", &[], None, vec![]);
            }
        }
    }
}
