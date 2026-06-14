//! The `rerouting` Callflow shape: alice → SUT → bob1 REJECTS (486) → the
//! b2bua re-targets bob2 (rejection-driven ADR-0017 failover via the infra's
//! decision engine — no timer, so the shape stays advance-free) → bob2
//! answers → ACK → BYE. Needs an Infra shape whose SUT is failover-capable
//! and whose Endpoint config binds a `bob2` role.

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::infra::InfraRuntime;
use crate::model::Input;
use crate::shape::{Anchor, CallflowShape};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ANCHORS: &[Anchor] = &[Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye];

/// Authoring parameters for the `rerouting` shape — the typed `input.extras` the
/// editor suggests. All optional (defaults reproduce the canonical 486 flow), so
/// a case that sets none behaves exactly as before.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ReroutingParams {
    /// SIP status bob1 rejects the first b-leg with, triggering the SUT's
    /// failover to bob2. Any 4xx–6xx the decision engine treats as a reroute
    /// trigger (default `486` Busy Here; e.g. `503` Service Unavailable).
    pub reject_status: u16,
    /// Reason phrase paired with `rejectStatus` (default `"Busy Here"`).
    pub reject_reason: String,
}

impl Default for ReroutingParams {
    fn default() -> Self {
        ReroutingParams { reject_status: 486, reject_reason: "Busy Here".to_string() }
    }
}

pub struct Rerouting;

#[async_trait(?Send)]
impl CallflowShape for Rerouting {
    fn id(&self) -> &str {
        "rerouting"
    }
    fn anchors(&self) -> &[Anchor] {
        ANCHORS
    }
    fn agents(&self) -> &[&str] {
        &["alice", "bob1", "bob2"]
    }
    fn params_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::to_value(schema_for!(ReroutingParams)).expect("ReroutingParams schema"))
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        let params: ReroutingParams = input.params();
        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");
        let bob2 = rt.agent("bob2");

        // alice INVITEs bob1 with bob2 as the failover candidate. The layout
        // realizes the candidate list on its wire (see `basic_call.rs`): a pinned
        // layout emits an `X-Api-Call` `routes` failover plan [bob1, bob2]; the
        // fake LB's scripted engine owns the same failover and ignores the header.
        // Either way bob1's rejection drives the reroute to bob2.
        let invite = rt.outgoing_invite(&["bob1", "bob2"], input, alice.invite(bob1).with_sdp(OFFER));
        let mut call = invite.send().await;

        // bob1 gets the first b-leg (anchor: bob1.initialInvite) and REJECTS with
        // the authored status (default 486), triggering the SUT's failover.
        let mut uas1 = bob1.receive("INVITE").await;
        rt.anchor("bob1", Anchor::InitialInvite, uas1.request());
        uas1.respond(params.reject_status, &params.reject_reason).await;

        // The SUT fails over: bob2 gets the rerouted b-leg
        // (anchor: bob2.initialInvite) and answers.
        let mut uas2 = bob2.receive("INVITE").await;
        rt.anchor("bob2", Anchor::InitialInvite, uas2.request());
        uas2.respond(200, "OK").with_sdp(ANSWER).await;
        let answer = call.expect(200).await;
        rt.anchor("alice", Anchor::Answer, &answer);

        // ACK lands on the WINNING leg (bob2).
        let mut dialog = call.ack().await;
        let ack = bob2.receive("ACK").await;
        rt.anchor("bob2", Anchor::Ack, ack.request());

        // alice hangs up.
        let mut bye = dialog.bye().await;
        let mut bye_uas = bob2.receive("BYE").await;
        rt.anchor("bob2", Anchor::Bye, bye_uas.request());
        bye_uas.respond(200, "OK").await;
        bye.expect(200).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Input;

    #[test]
    fn params_default_to_the_canonical_486_flow() {
        let p: ReroutingParams = Input::default().params();
        assert_eq!(p.reject_status, 486);
        assert_eq!(p.reject_reason, "Busy Here");
    }

    #[test]
    fn authored_extras_override_the_reject_status() {
        let mut input = Input::default();
        input.extras.insert("rejectStatus".into(), serde_json::json!(503));
        input.extras.insert("rejectReason".into(), serde_json::json!("Service Unavailable"));
        let p: ReroutingParams = input.params();
        assert_eq!(p.reject_status, 503);
        assert_eq!(p.reject_reason, "Service Unavailable");
    }

    #[test]
    #[should_panic(expected = "invalid shape params")]
    fn a_typo_in_extras_panics_loudly_not_silently_defaults() {
        let mut input = Input::default();
        input.extras.insert("rejectStatux".into(), serde_json::json!(503)); // typo
        let _: ReroutingParams = input.params();
    }
}
