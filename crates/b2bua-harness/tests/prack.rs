//! End-to-end PRACK relay (port of `tests/scenarios/prack.ts`, the default
//! end-to-end path — *not* the deferred `fake-prack` 18x-management strategy).
//!
//! Reliable provisional handling (RFC 3262): bob sends a 183 with
//! `Require: 100rel` + `RSeq`, alice PRACKs it (with `RAck`), bob 200s the
//! PRACK, then 200s the INVITE. The B2BUA bridges two dialogs and relays the
//! PRACK / 200(PRACK) transparently (the `relay-prack` + `relay-non-invite-200`
//! rules) — it does not implement the RFC 3262 state machine itself, so the
//! reliable-provisional bookkeeping (RSeq/RAck) is end-to-end between the UAs.
//!
//! ```text
//!   INVITE → 100 → 183(100rel,RSeq) → PRACK(RAck) → 200(PRACK)
//!          → 200(INVITE) → ACK → BYE → 200(BYE)
//! ```

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

#[tokio::test]
async fn prack_reliable_provisional_relayed_end_to_end() {
    let h = Harness::with_transit_delay("b2bua-prack", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5083", "127.0.0.1", 5073).await;

    // Alice INVITEs (offer in the INVITE) advertising 100rel support.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;

    // Bob receives the INVITE and answers reliably: 183 with Require:100rel,
    // RSeq and the SDP answer.
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;

    // Alice sees the 183 with 100rel intact (the B2BUA relays it transparently).
    let p183 = call.expect(183).await;
    assert_eq!(
        get_header(&p183.headers, "require").as_deref(),
        Some("100rel"),
        "Require: 100rel relayed to alice",
    );
    assert!(get_header(&p183.headers, "rseq").is_some(), "RSeq relayed to alice");

    // Alice PRACKs the reliable 183 on the early dialog.
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_rack("1 1 INVITE")
        .send()
        .await;

    // Bob receives the relayed PRACK and 200s it.
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    // Bob answers the INVITE; alice gets the 200 and ACKs.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Teardown.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
