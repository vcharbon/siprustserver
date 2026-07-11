//! The `rerouting` Callflow shape: alice → SUT → bob1 REJECTS (486) → the
//! b2bua re-targets bob2 (rejection-driven ADR-0017 failover via the infra's
//! decision engine — no timer, so the shape stays advance-free) → bob2
//! answers → ACK → BYE. Needs an Infra shape whose SUT is failover-capable
//! and whose Endpoint config binds a `bob2` role.

use async_trait::async_trait;

use crate::infra::InfraRuntime;
use crate::model::Input;
use crate::shape::{Anchor, CallflowShape};

// The typed authoring parameters live next to the shape's descriptor — its ONE
// declaration — in `e2e_model::registry` (which also splices their schema over
// the Test case's `extras`); re-exported here so `shapes::rerouting::ReroutingParams`
// keeps resolving.
pub use e2e_model::ReroutingParams;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The functional body of the `rerouting` shape (descriptor + params schema in
/// `e2e_model::registry`).
pub struct Rerouting;

#[async_trait(?Send)]
impl CallflowShape for Rerouting {
    fn agents(&self) -> &[&str] {
        &["alice", "bob1", "bob2"]
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
        bob1.receive("ACK").await; // the SUT completes bob1's reject txn (§17.1.1.3)

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
