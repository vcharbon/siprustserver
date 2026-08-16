//! End-to-end PRACK relay (port of `tests/scenarios/prack.ts`, the default
//! end-to-end path — *not* the deferred `fake-prack` 18x-management strategy).
//!
//! Reliable provisional handling (RFC 3262) with no 18x-management policy
//! declared: the B2BUA is FULLY transparent on reliability. `Supported: 100rel`
//! reaches the callee, the callee's reliable 18x reaches the caller as a
//! reliable provisional, the caller's PRACK reaches the callee, and its 200
//! comes back. The one number NOT carried across is `RSeq`: it is
//! per-INVITE-transaction sequencing (like CSeq), so the caller is shown this
//! stack's own and the PRACK's `RAck` translates back onto the callee's.
//!
//! ```text
//!   INVITE(Supported:100rel) → 100 → 183(100rel,RSeq) → PRACK(RAck) → 200(PRACK)
//!          → 200(INVITE) → ACK → BYE → 200(BYE)
//! ```

use b2bua_harness::B2buaSut;
use scenario_harness::{Harness, ServerTxn};
use sip_message::header::{HeaderName, RAck, RSeq, Require, Supported};
use sip_message::types::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Bob's own sequence — deliberately far from any number this stack mints
/// first, so "the caller was shown ours" is provable rather than coincidental.
const BOB_RSEQ: u32 = 4711;

fn rseq_of(resp: &SipResponse) -> u32 {
    resp.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value()
}

fn reliable_183(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(ANSWER)
}

#[tokio::test]
async fn prack_reliable_provisional_relayed_end_to_end() {
    let h = Harness::with_transit_delay("b2bua-prack", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    // Alice INVITEs (offer in the INVITE) advertising 100rel support.
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // The callee is told the caller supports reliable provisionals — without
    // that it has no licence to send one (RFC 3262 §3).
    let mut uas = bob.receive("INVITE").await;
    assert!(
        uas.request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "Supported: 100rel reaches the callee",
    );

    // Bob answers reliably: 183 with Require:100rel, his own RSeq and the SDP.
    reliable_183(&mut uas).await;

    // Alice sees a RELIABLE provisional — under this stack's own RSeq.
    let p183 = call.expect(183).await;
    assert!(
        p183.header::<Require>().expect("a Require").expect("readable Require").contains("100rel"),
        "Require: 100rel relayed to alice",
    );
    assert_ne!(
        rseq_of(&p183),
        BOB_RSEQ,
        "the a-facing RSeq is this stack's own per-transaction sequence, not the callee's",
    );

    // Alice PRACKs the provisional she was shown — RAck built from HER RSeq.
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");

    // Bob receives the relayed PRACK naming HIS number, and 200s it.
    let mut bob_prack = bob.receive("PRACK").await;
    let relayed_rack =
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck");
    assert_eq!(
        relayed_rack.rseq(),
        BOB_RSEQ,
        "the RAck translates back onto the sequence the callee stated",
    );
    bob_prack.respond(200, "OK").await;
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

/// A retransmitted reliable provisional (RFC 3262 §3: the callee re-sends until
/// PRACKed) reaches the caller as the SAME provisional — the a-facing number is
/// recalled, not minted again, so the caller sees a retransmission rather than a
/// second reliable 18x it would have to PRACK separately.
#[tokio::test]
async fn a_retransmitted_reliable_provisional_keeps_its_a_facing_number() {
    let h = Harness::with_transit_delay("b2bua-prack-retransmit", 0);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    reliable_183(&mut uas).await;
    let first = call.expect(183).await;
    reliable_183(&mut uas).await;
    let again = call.expect(183).await;
    assert_eq!(
        rseq_of(&again),
        rseq_of(&first),
        "the retransmission carries the number the caller already saw",
    );

    // One PRACK retires both — they are one provisional.
    let mut prack = call.try_prack(&again).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

/// A provisional the callee did NOT send reliably names no sequence at all —
/// the stack mints a number only for what it relays as a reliable provisional.
#[tokio::test]
async fn an_unreliable_provisional_carries_no_rseq() {
    let h = Harness::with_transit_delay("b2bua-prack-unreliable", 0);
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;

    let ringing = call.expect(180).await;
    assert_eq!(ringing.raw(HeaderName::RSeq).next(), None, "an unreliable 18x names no sequence");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
