//! The `basic-call` Callflow shape: alice → SUT(ingress=LB) → b2bua → SUT → bob1,
//! 180/200, ACK, alice-BYE. Advance-free (message-driven), so it ports unchanged
//! over fake and real Infra shapes.

use async_trait::async_trait;

use crate::infra::InfraRuntime;
use crate::shape::{Anchor, CallflowShape, Input};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ANCHORS: &[Anchor] = &[Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye];

pub struct BasicCall;

#[async_trait(?Send)]
impl CallflowShape for BasicCall {
    fn id(&self) -> &str {
        "basic-call"
    }
    fn anchors(&self) -> &[Anchor] {
        ANCHORS
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        let sut = rt.sut_ingress;
        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");

        // alice INVITEs through the SUT ingress (the LB), carrying any From/To/
        // R-URI the Test case supplied. The LB HRW-routes to the b2bua, which
        // bridges the b-leg back through the LB to bob1.
        let mut invite = alice.invite(bob1).with_sdp(OFFER).through(sut);
        // On the real cluster the worker's engine routes by the caller-pinned
        // `X-Api-Call.destination` (fallback = its in-cluster B2BUA_DEST, which
        // is NOT our bob); the fake engine ignores the header. Same shape body
        // either way — the infra decides whether the pin is needed.
        if let Some(dest) = rt.api_call_destination() {
            invite = invite.with_header(
                "X-Api-Call",
                &format!(r#"{{"destination":{{"host":"{}","port":{}}}}}"#, dest.ip(), dest.port()),
            );
        }
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
