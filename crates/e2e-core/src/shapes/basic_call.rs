//! The `basic-call` Callflow shape: alice → SUT(ingress=LB) → b2bua → SUT → bob1,
//! 180/200, ACK, alice-BYE. Advance-free (message-driven), so it ports unchanged
//! over fake and real Infra shapes.

use async_trait::async_trait;

use crate::infra::InfraRuntime;
use crate::model::Input;
use crate::shape::{Anchor, CallflowShape};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The functional body of the `basic-call` shape (its descriptor — id, anchors
/// — is declared once in `e2e_model::registry`).
pub struct BasicCall;

#[async_trait(?Send)]
impl CallflowShape for BasicCall {
    fn agents(&self) -> &[&str] {
        &["alice", "bob1"]
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");

        // alice INVITEs bob1. The LAYOUT realizes this logical INVITE on its wire
        // (the real cluster's `X-Api-Call` pin, the register proxy's registered-
        // AOR R-URI, or nothing for the fake LB+b2bua) and applies the Test case's
        // From/To/R-URI — so the SAME body runs over every infra (ADR-0018).
        let invite = rt.outgoing_invite(&["bob1"], input, alice.invite(bob1).with_sdp(OFFER));
        let mut call = invite.send().await;

        // INITIAL INVITE arrives at bob1 (anchor: bob1.initialInvite).
        let mut uas = bob1.receive("INVITE").await;
        rt.anchor("bob1", Anchor::InitialInvite, uas.request());
        uas.respond(180, "Ringing").await;
        call.expect(180).await;

        // ANSWER (anchor: alice.answer — the 200 as the SUT delivered it).
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        let answer = call.expect(200).await;
        rt.anchor("alice", Anchor::Answer, &answer);

        // ACK (anchor: bob1.ack).
        let mut dialog = call.ack().await;
        let ack = bob1.receive("ACK").await;
        rt.anchor("bob1", Anchor::Ack, ack.request());

        // alice hangs up (BYE; anchor: bob1.bye).
        let mut bye = dialog.bye().await;
        let mut bye_uas = bob1.receive("BYE").await;
        rt.anchor("bob1", Anchor::Bye, bye_uas.request());
        bye_uas.respond(200, "OK").await;
        bye.expect(200).await;
    }
}
