//! The `rerouting` Callflow shape: alice → SUT → bob1 REJECTS (486) → the
//! b2bua re-targets bob2 (rejection-driven ADR-0017 failover via the infra's
//! decision engine — no timer, so the shape stays advance-free) → bob2
//! answers → ACK → BYE. Needs an Infra shape whose SUT is failover-capable
//! and whose Endpoint config binds a `bob2` role.

use async_trait::async_trait;

use crate::infra::InfraRuntime;
use crate::shape::{Anchor, CallflowShape, Input};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ANCHORS: &[Anchor] = &[Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye];

pub struct Rerouting;

#[async_trait(?Send)]
impl CallflowShape for Rerouting {
    fn id(&self) -> &str {
        "rerouting"
    }
    fn anchors(&self) -> &[Anchor] {
        ANCHORS
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        let sut = rt.sut_ingress;
        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");
        let bob2 = rt.agent("bob2");

        let mut invite = alice.invite(bob1).with_sdp(OFFER).through(sut);
        if let Some(from) = &input.from {
            invite = invite.from(from);
        }
        if let Some(to) = &input.to {
            invite = invite.to(to);
        }
        if let Some(ruri) = &input.ruri {
            invite = invite.ruri(ruri);
        }
        let mut call = invite.send().await;

        // bob1 gets the first b-leg (anchor: bob1.initialInvite) and REJECTS.
        let mut uas1 = bob1.receive("INVITE").await;
        rt.anchor("bob1", Anchor::InitialInvite, uas1.request());
        uas1.respond(486, "Busy Here").await;

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
