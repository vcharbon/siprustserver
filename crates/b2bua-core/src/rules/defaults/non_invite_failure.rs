//! The non-2xx finals to non-INVITE requests other than BYE: a relayed
//! request's, relayed back to its requester; a relayed PRACK's, answered on
//! its sender's leg by what the PRACK carried; and this stack's own PRACK's or
//! NOTIFY's, which deny nothing beyond their transaction or subscription.

use b2bua_sdk::model::{Match, RuleAction, RuleContext, RuleDefinition};

use super::core_rules::{no_transform, ok, rule};

/// `relay-non-invite-failure`: a relayed non-INVITE request's non-2xx final
/// is relayed back to its requester (RFC 3261 §8.1.3.3), the sibling of
/// `relay-reinvite-response` and `relay-non-invite-200`; a relayed PRACK's
/// has the rules below. It matches only a final answering a relay record on
/// the source dialog: this stack's own requests (keepalive OPTIONS, its own
/// PRACK, a REFER-progress NOTIFY) keep their own rules, so a keepalive 481
/// still reaches `handle-481`, which this rule outranks for relays alone. Like
/// a failed re-INVITE (§14.1), the failure leaves the dialog and the call as
/// they were. A BYE is answered locally and relayed by no record.
pub(super) fn relay_non_invite_failure_rule() -> RuleDefinition {
    rule(
        "relay-non-invite-failure",
        &["handle-481"],
        Match::response()
            .methods(&["OPTIONS", "INFO", "UPDATE", "REFER", "MESSAGE", "SUBSCRIBE", "NOTIFY"])
            .filter(|ctx| {
                ctx.response().is_some_and(|r| r.status() >= 300) && ctx.answers_relayed_request()
            }),
        |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
    )
}

/// Whether the current response is a failure final to a relayed PRACK that
/// carried an offer (`Some(true)`), or one that carried an answer or no
/// description (`Some(false)`); `None` for anything else.
fn relayed_prack_failure_offered(ctx: &RuleContext) -> Option<bool> {
    ctx.response().filter(|r| r.status() >= 300)?;
    ctx.answered_record().filter(|r| r.is_relay()).map(|r| r.offered)
}

/// `answer-relayed-prack-failure`: a PRACK is relayed only where it matches a
/// reliable provisional this stack showed its sender unacknowledged, and such
/// a PRACK MUST draw a 2xx (RFC 3262 §3): a non-2xx final to the relayed copy
/// of one carrying no offer is answered 200 on the sender's leg, never relayed.
pub(super) fn answer_relayed_prack_failure_rule() -> RuleDefinition {
    rule(
        "answer-relayed-prack-failure",
        &[],
        Match::response()
            .method("PRACK")
            .filter(|ctx| relayed_prack_failure_offered(ctx) == Some(false)),
        |_ctx| ok(vec![RuleAction::AnswerRelayOriginator { status: 200, reason: "OK".into() }]),
    )
}

/// `relay-offered-prack-failure`: a non-2xx final to a relayed PRACK that
/// carried an offer (RFC 3262 §5) is relayed to its sender: the offer is
/// refused end to end, as a relayed UPDATE's is.
pub(super) fn relay_offered_prack_failure_rule() -> RuleDefinition {
    rule(
        "relay-offered-prack-failure",
        &[],
        Match::response()
            .method("PRACK")
            .filter(|ctx| relayed_prack_failure_offered(ctx) == Some(true)),
        |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
    )
}

/// `absorb-own-request-failure`: a non-2xx final to a request THIS STACK
/// originated — its own PRACK toward a responder whose provisional the
/// originator was never shown reliably, or a REFER-progress NOTIFY — records
/// as this stack's own, and denies one transaction or one subscription, never
/// the dialog: a PRACK's 481 is RFC 3262 §3's ordinary answer once the
/// responder no longer holds that provisional unacknowledged; a NOTIFY's that
/// removes its subscription ended it before the rules (RFC 6665 §4.2.2).
/// Nothing is owed and nothing moves.
pub(super) fn absorb_own_request_failure_rule() -> RuleDefinition {
    rule(
        "absorb-own-request-failure",
        &[],
        Match::response().methods(&["PRACK", "NOTIFY"]).filter(|ctx| {
            ctx.response().is_some_and(|r| r.status() >= 300) && !ctx.answers_relayed_request()
        }),
        |_ctx| ok(vec![]),
    )
}
