//! The non-2xx finals to a PRACK or a NOTIFY that are not relayed back to a
//! requester: a relayed PRACK's, which its sender is owed a 2xx for, and
//! this stack's own PRACK's or NOTIFY's, which deny nothing beyond their
//! transaction or subscription. A relayed NOTIFY's failure, like any other
//! relayed non-INVITE request's, is `relay-non-invite-failure`'s to relay.

use b2bua_sdk::model::{Match, RuleAction, RuleDefinition};

use super::core_rules::{ok, rule};

/// `answer-relayed-prack-failure`: a PRACK is relayed only where it matches a
/// reliable provisional this stack showed its sender unacknowledged, and such
/// a PRACK MUST draw a 2xx (RFC 3262 §3): a non-2xx final to the relayed copy
/// is answered 200 on the sender's leg, never relayed.
pub(super) fn answer_relayed_prack_failure_rule() -> RuleDefinition {
    rule(
        "answer-relayed-prack-failure",
        &[],
        Match::response().method("PRACK").filter(|ctx| {
            ctx.response().is_some_and(|r| r.status() >= 300) && ctx.answers_relayed_request()
        }),
        |_ctx| ok(vec![RuleAction::AnswerRelayOriginator { status: 200, reason: "OK".into() }]),
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
