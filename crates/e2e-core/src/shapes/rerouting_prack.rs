//! The `rerouting-prack` Callflow shape: `rerouting` + a RELIABLE provisional
//! on the winning leg (RFC 3262) — bob2 answers 183 `Require: 100rel`/`RSeq`,
//! alice PRACKs (`RAck`), bob2 200s the PRACK, then answers. The b2bua relays
//! PRACK end-to-end (`relay-prack` / `relay-non-invite-200`); the RSeq/RAck
//! bookkeeping is UA-to-UA. Entirely message-driven (advance-free).

use async_trait::async_trait;
use sip_message::generators::InDialogMethod;

use crate::infra::InfraRuntime;
use crate::shape::{Anchor, CallflowShape, Input};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Prack,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];

pub struct ReroutingPrack;

#[async_trait(?Send)]
impl CallflowShape for ReroutingPrack {
    fn id(&self) -> &str {
        "rerouting-prack"
    }
    fn anchors(&self) -> &[Anchor] {
        ANCHORS
    }

    async fn run(&self, rt: &mut InfraRuntime, input: &Input) {
        let sut = rt.sut_ingress;
        let alice = rt.agent("alice");
        let bob1 = rt.agent("bob1");
        let bob2 = rt.agent("bob2");

        // A UAC that intends to PRACK advertises 100rel support on the INVITE
        // (RFC 3262 §3) — without it, bob2's reliable 183 (relayed end-to-end
        // by the SUT) would be a MUST-004 "reliable 1xx without client opt-in"
        // violation by every UAS on the path.
        let mut invite = alice
            .invite(bob1)
            .with_sdp(OFFER)
            .with_header("Supported", "100rel")
            .through(sut);
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

        // bob1 rejects; the SUT fails over to bob2.
        let mut uas1 = bob1.receive("INVITE").await;
        rt.anchor("bob1", Anchor::InitialInvite, uas1.request());
        uas1.respond(486, "Busy Here").await;

        let mut uas2 = bob2.receive("INVITE").await;
        rt.anchor("bob2", Anchor::InitialInvite, uas2.request());

        // bob2 answers RELIABLY: 183 + Require:100rel + RSeq (+ SDP answer).
        uas2.respond(183, "Session Progress")
            .with_header("Require", "100rel")
            .with_header("RSeq", "1")
            .with_sdp(ANSWER)
            .await;
        let p183 = call.expect(183).await;
        rt.anchor("alice", Anchor::FirstProvisional, &p183);

        // alice PRACKs the reliable 183 on the early dialog; bob2 200s it.
        let mut prack = call
            .send_request(InDialogMethod::Prack)
            .with_rack("1 1 INVITE")
            .send()
            .await;
        let mut prack_uas = bob2.receive("PRACK").await;
        rt.anchor("bob2", Anchor::Prack, prack_uas.request());
        prack_uas.respond(200, "OK").await;
        prack.expect(200).await;

        // bob2 answers the INVITE; alice ACKs the winning leg.
        uas2.respond(200, "OK").with_sdp(ANSWER).await;
        let answer = call.expect(200).await;
        rt.anchor("alice", Anchor::Answer, &answer);
        let mut dialog = call.ack().await;
        let ack = bob2.receive("ACK").await;
        rt.anchor("bob2", Anchor::Ack, ack.request());

        // Teardown.
        let mut bye = dialog.bye().await;
        let mut bye_uas = bob2.receive("BYE").await;
        rt.anchor("bob2", Anchor::Bye, bye_uas.request());
        bye_uas.respond(200, "OK").await;
        bye.expect(200).await;
    }
}
