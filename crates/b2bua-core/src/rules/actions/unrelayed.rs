//! The final an in-dialog request draws when the relay it was given to has no
//! dialog to carry it, or builds a copy that cannot leave ([`Unrelayed`]): a
//! relay never leaves its sender to run out Timer F (RFC 3261 §8.2, §17.2.2).

use call::Call;
use sip_message::datagram::max_udp_payload;
use sip_message::header::HeaderName;
use sip_message::ip_literal::ip_socket_addr;
use sip_message::{Method, SipHeader, SipRequest, SipStr};

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
    /// The request's lines make no request toward the peer dialog: 500 with
    /// no `Retry-After` (RFC 3261 §21.5.1), since a retry carries the same
    /// lines.
    Unbuildable,
    /// The request comes from a leg its service owns (ADR-0014), and no rule
    /// of that service took it: 403 (§21.4.4).
    ServiceOwned,
    /// Its copy toward the peer is past the largest UDP payload, though the
    /// request fit: 513 (§21.5.14). A refused re-INVITE leaves the session
    /// unchanged (§14.1); a non-INVITE draws no 408 (RFC 4320 §4.2).
    TooLarge,
}

/// Whether `copy`, bound for `dest`, is past the largest UDP payload of an IP
/// literal's family. A name next hop is judged where it is sent.
pub(super) fn too_large(copy: &SipRequest, dest: &(String, u16)) -> bool {
    ip_socket_addr(&dest.0, dest.1).is_some_and(|at| copy.image().len() > max_udp_payload(at))
}

impl ActionExecutor<'_> {
    /// Answer the turn's request for `why`; an ACK is never answered
    /// (§17.1.1.3), and a response has no answer.
    pub(super) fn refuse_unrelayed(
        &mut self,
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
            Unrelayed::Unbuildable => {
                self.respond(call, fx, ctx, 500, "Server Internal Error", &[], None, vec![]);
            }
            Unrelayed::ServiceOwned => {
                self.respond(call, fx, ctx, 403, "Forbidden", &[], None, vec![]);
            }
            Unrelayed::TooLarge => {
                self.respond(call, fx, ctx, 513, "Message Too Large", &[], None, vec![]);
            }
        }
    }
}
