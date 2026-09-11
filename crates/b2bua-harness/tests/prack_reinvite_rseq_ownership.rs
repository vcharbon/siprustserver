//! A reliable provisional to a RELAYED RE-INVITE is numbered by this stack, not
//! by the callee (issue 109).
//!
//! RFC 3262 scopes reliable provisionals by METHOD — §3 "does not allow
//! reliable provisional responses for any method but INVITE" — and a re-INVITE
//! is an INVITE. The §3 sentence barring one on a request that already carries
//! a To tag is written for a PROXY and says so ("unlike a UAS"); a B2BUA is the
//! UAS of its a-face, and RFC 6141 §4.6 has a UAS answer a target-refresh
//! re-INVITE reliably. So this flow is ordinary compliant SIP.
//!
//! `RSeq` is per-INVITE-transaction sequencing (§3: "The RSeq numbering space
//! is within a single transaction"), so the number the caller sees on the
//! a-face is this stack's own — exactly as on the initial INVITE
//! (`prack.rs`) — and the PRACK's `RAck` translates back onto the callee's.
//! Without that mint the books hold nothing, and the caller's PRACK reaches the
//! callee only by the accident that the number it names is his.
//!
//! ```text
//!   INVITE → 180 → 200 → ACK
//!   re-INVITE(Supported:100rel) → 183(100rel,RSeq) → PRACK(RAck) → 200(PRACK)
//!          → 200(INVITE) → ACK → BYE → 200(BYE)
//! ```

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{RAck, RSeq, Require, Supported};
use sip_message::types::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";
/// The callee-originated round: bob offers, alice answers. RFC 4566 §5.2 keeps
/// each party's `o=` identity and moves only its sess-version.
const REOFFER_B: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n";
const REANSWER_B: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30003 RTP/AVP 0\r\n";

/// Bob's own sequence on the re-INVITE transaction — deliberately far from any
/// number this stack mints first, so "the caller was shown ours" is provable
/// rather than coincidental.
const BOB_RSEQ: u32 = 4711;

fn rseq_of(resp: &SipResponse) -> u32 {
    resp.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value()
}

#[tokio::test]
async fn a_reliable_provisional_to_a_relayed_reinvite_is_numbered_by_this_stack() {
    let h = Harness::with_transit_delay("b2bua-prack-reinvite-rseq", 0);
    let alice = h.agent("alice", "127.0.0.1:5045").await;
    let bob = h.agent("bob", "127.0.0.1:5046").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5046).start(&h, "b2bua", "127.0.0.1:5047").await;

    // ── an ordinary call, established without reliability in play ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs, advertising reliable provisionals ──
    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Supported", "100rel")
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .send()
        .await;

    // The callee is told the caller supports them — without that he has no
    // licence to answer reliably (RFC 3262 §3).
    let mut re_uas = bob.receive("INVITE").await;
    assert!(
        re_uas
            .request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "Supported: 100rel reaches the callee on the relayed re-INVITE",
    );

    // Bob answers reliably: 183 with Require:100rel, HIS OWN RSeq, and the SDP
    // answer to alice's re-offer.
    re_uas
        .respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(REANSWER)
        .await;

    // Alice sees a RELIABLE provisional — under this stack's own sequence.
    let p183 = reinv.expect(183).await;
    assert!(
        p183.header::<Require>().expect("a Require").expect("readable Require").contains("100rel"),
        "Require: 100rel relayed to alice",
    );
    let shown = rseq_of(&p183);
    assert_ne!(
        shown, BOB_RSEQ,
        "the a-facing RSeq on a relayed re-INVITE is this stack's own per-transaction sequence, \
         not the callee's (RFC 3262 §3; issue 109)",
    );

    // Alice PRACKs the provisional she was shown — RAck built from HER number
    // and the re-INVITE's CSeq (RFC 3262 §7.2).
    let mut prack = alice_dialog
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown} {} INVITE", p183.cseq().seq()))
        .send()
        .await;

    // Bob receives the relayed PRACK naming HIS number, and 200s it.
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        BOB_RSEQ,
        "the RAck translates back onto the sequence the callee stated",
    );
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;

    // ── bob answers the re-INVITE; alice ACKs the 2xx (her offer was in the
    // re-INVITE, so the ACK is bodyless — RFC 3261 §13.2.2.4) ──
    let reinvite_cseq = p183.cseq().seq();
    re_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    alice_dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}

/// Alice's own sequence on a CALLEE-originated re-INVITE — the mirror of
/// [`BOB_RSEQ`], and equally far from any number this stack mints first.
const ALICE_RSEQ: u32 = 9271;

/// The same invariant from the other face: a B2BUA is the UAS of whichever
/// face it relays a response TOWARD, so the sequence it shows there is its own
/// whichever end originated the re-INVITE. Bob re-INVITEs, alice answers
/// reliably with HER number, and bob must be shown this stack's.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   ← re-INVITE(Supported:100rel) ← 183(100rel,RSeq) ← PRACK(RAck) ← 200(PRACK)
///          ← 200(INVITE) ← ACK → BYE → 200(BYE)
/// ```
#[tokio::test]
async fn a_reliable_provisional_to_a_callee_originated_reinvite_is_numbered_by_this_stack() {
    let h = Harness::with_transit_delay("b2bua-prack-reinvite-rseq-callee", 0);
    let alice = h.agent("alice", "127.0.0.1:5048").await;
    let bob = h.agent("bob", "127.0.0.1:5053").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5053).start(&h, "b2bua", "127.0.0.1:5054").await;

    // ── an ordinary call, established without reliability in play ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // ── bob re-INVITEs toward alice, advertising reliable provisionals ──
    let mut reinv = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER_B)
        .with_header("Supported", "100rel")
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .send()
        .await;

    // Alice is told the originator supports them — without that she has no
    // licence to answer reliably (RFC 3262 §3).
    let mut alice_uas = alice.receive("INVITE").await;
    assert!(
        alice_uas
            .request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "Supported: 100rel reaches the caller on the relayed re-INVITE",
    );

    // Alice answers reliably with HER OWN RSeq and the SDP answer.
    alice_uas
        .respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &ALICE_RSEQ.to_string())
        .with_sdp(REANSWER_B)
        .await;

    // Bob sees a RELIABLE provisional — under this stack's own sequence.
    let p183 = reinv.expect(183).await;
    assert!(
        p183.header::<Require>().expect("a Require").expect("readable Require").contains("100rel"),
        "Require: 100rel relayed to bob",
    );
    let shown = rseq_of(&p183);
    assert_ne!(
        shown, ALICE_RSEQ,
        "the RSeq shown on the face a response is relayed TOWARD is this stack's own \
         per-transaction sequence, not the far party's (RFC 3262 §3; issue 109)",
    );

    // Bob PRACKs the provisional he was shown; it translates back onto alice's.
    let mut prack = bob_dialog
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown} {} INVITE", p183.cseq().seq()))
        .send()
        .await;

    let mut alice_prack = alice.receive("PRACK").await;
    assert_eq!(
        alice_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        ALICE_RSEQ,
        "the RAck translates back onto the sequence the caller stated",
    );
    alice_prack.respond(200, "OK").await;
    prack.expect(200).await;

    // ── alice answers the re-INVITE; bob ACKs the 2xx (his offer was in the
    // re-INVITE, so the ACK is bodyless — RFC 3261 §13.2.2.4) ──
    let reinvite_cseq = p183.cseq().seq();
    alice_uas.respond(200, "OK").with_sdp(REANSWER_B).await;
    reinv.expect(200).await;
    bob_dialog.ack_for(reinvite_cseq, None).await;
    alice.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}
