//! The builders of a B2BUA under a scripted decision engine: where it routes
//! its calls, and with which features active.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;

use crate::{B2buaSut, B2buaSutBuilder};

impl B2buaSut {
    /// Builder for a B2BUA that routes every call to `dest` (the common case).
    pub fn route_all_to(dest_host: &str, dest_port: u16) -> B2buaSutBuilder {
        Self::builder(Arc::new(ScriptedDecisionEngine::route_all_to(dest_host, dest_port)))
    }

    /// Builder for a B2BUA that routes every call to `dest` and authorizes REFER
    /// transfers via the default `X-Api-Call`-keyed `/call/refer` behavior (the
    /// REFER-scenario constructor).
    pub fn route_all_with_refer(dest_host: &str, dest_port: u16) -> B2buaSutBuilder {
        Self::builder(Arc::new(ScriptedDecisionEngine::route_all_with_refer(dest_host, dest_port)))
    }

    /// Builder for a B2BUA whose engine honors the full inbound `X-Api-Call`
    /// control surface — a single `destination` pin, an ADR-0017 `routes`
    /// failover plan walked on b-leg rejection (`/call/failure` pops the next
    /// route), per-call `call_limiter` admission and the `features.refer` arm;
    /// plan-less traffic falls back to routing to `dest`. Every activation is
    /// the CALLER's to state — nothing is implied — which is what a replayed
    /// document needs. This is the SUT the loadgen rerouting scenarios exercise
    /// under the `api-call-pin` egress policy.
    pub fn route_api_call(dest_host: &str, dest_port: u16) -> B2buaSutBuilder {
        Self::builder(Arc::new(ScriptedDecisionEngine::route_by_api_call(dest_host, dest_port)))
    }

    /// Builder for a B2BUA that routes every call to `dest` with the
    /// `relayFirst18xTo180` feature active under `strategy` (suppress / fake-prack).
    pub fn route_all_to_with_18x(
        dest_host: &str,
        dest_port: u16,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> B2buaSutBuilder {
        let dest = (dest_host.to_string(), dest_port);
        Self::builder(Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    b2bua::decision::NewCallResponse::Route(
                        b2bua::decision::test_adapter::route_to_with_18x(&dest.0, dest.1, strategy),
                    )
                })
                .build(),
        ))
    }

    /// Builder for a B2BUA that routes every call to `dest` with the
    /// `relayFirst18xTo180` feature active under `strategy` and an explicit
    /// [`call::features::Relay18xMessages`] policy.
    pub fn route_all_to_with_18x_messages(
        dest_host: &str,
        dest_port: u16,
        strategy: call::features::RelayFirst18xStrategy,
        messages: call::features::Relay18xMessages,
    ) -> B2buaSutBuilder {
        let dest = (dest_host.to_string(), dest_port);
        Self::builder(Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    b2bua::decision::NewCallResponse::Route(
                        b2bua::decision::test_adapter::route_to_with_18x_messages(
                            &dest.0, dest.1, strategy, messages,
                        ),
                    )
                })
                .build(),
        ))
    }

    /// Builder for a B2BUA that routes the call to `dest_port` (bob1) with the
    /// `relayFirst18xTo180` feature active under `strategy` and a `callback_context`
    /// set (the failover-capable marker), and fails over via `/call/failure` to
    /// `failover_port` (bob2) with `failover_ruri` as the new Request-URI. Mirrors
    /// the TS `on_failure: { action: "failover", destination, new_ruri }` instruction.
    pub fn route_all_to_with_18x_failover(
        dest_host: &str,
        dest_port: u16,
        failover_port: u16,
        failover_ruri: &str,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> B2buaSutBuilder {
        use b2bua::decision::test_adapter::route_to_with_18x;
        use b2bua::decision::{CallFailureResponse, NewCallResponse};
        let primary = (dest_host.to_string(), dest_port);
        let failover = (dest_host.to_string(), failover_port);
        let failover_ruri = failover_ruri.to_string();
        Self::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    let mut r = route_to_with_18x(&primary.0, primary.1, strategy);
                    r.callback_context = Some("failover-test".into());
                    // The TS relies on the 30 s platform no-answer default the Rust
                    // default route omits — set it so the no-answer failover fires.
                    r.no_answer_ms = Some(30_000);
                    NewCallResponse::Route(r)
                })
                .on_failure(move |_req| {
                    let mut r = route_to_with_18x(&failover.0, failover.1, strategy);
                    r.new_ruri = Some(failover_ruri.clone());
                    CallFailureResponse::Route(r)
                })
                .build(),
        ))
    }
}
