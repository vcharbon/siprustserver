//! The timeouts of requests this stack originated itself that end only what
//! the request was for: the call stays as it was. Registered between the straggler absorptions and
//! the terminating rules of [`super::core_rules`], ahead of `handle-timeout`.

use b2bua_sdk::model::{Match, RuleDefinition};

use super::core_rules::{ok, rule};

/// The own-request timeout rules, in registration (priority) order.
pub(super) fn own_request_timeouts() -> [RuleDefinition; 2] {
    [
        // The timeout of a PRACK this stack originated itself (RFC 3261
        // §17.1.2.2 Timer F) denies that one acknowledgement, as its non-2xx
        // final does (`absorb-own-request-failure`): the dialog and the call
        // stay as they were. A relayed PRACK's timeout keeps `handle-timeout`.
        rule(
            "absorb-own-prack-timeout",
            &[],
            Match::timeout().filter(|ctx| {
                ctx.timeout_method().is_some_and(|m| m.eq_ignore_ascii_case("PRACK"))
                    && ctx.call.own_prack_branch(ctx.timeout_branch().unwrap_or_default())
            }),
            |_ctx| ok(vec![]),
        )
        .runs_while_terminating(),
        // The timeout of a NOTIFY this stack originated itself on a leg still
        // up ends that NOTIFY's subscription alone (RFC 6665 §4.2.2), which a
        // machine owning it reads first: the INVITE usage sharing the dialog
        // stands (RFC 5057 §5.2). A relayed NOTIFY's timeout and a leg going
        // away keep `handle-timeout`.
        rule(
            "absorb-own-notify-timeout",
            &[],
            Match::timeout().filter(|ctx| {
                let (Some(method), Some(cseq), Some(leg)) =
                    (ctx.timeout_method(), ctx.timeout_cseq(), ctx.source_leg())
                else {
                    return false;
                };
                method.eq_ignore_ascii_case("NOTIFY")
                    && !call::helpers::leg_is_going_away(ctx.call.state(), leg)
                    && !call::helpers::leg_relays(leg, "NOTIFY", i64::from(cseq))
            }),
            |_ctx| ok(vec![]),
        ),
    ]
}
