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

use std::net::SocketAddr;

use b2bua_harness::B2buaSut;
use scenario_harness::{Harness, ServerTxn, WaiverScope};
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

/// RFC 3262 §4: a PRACK matching no unacknowledged reliable provisional takes
/// `481`, and the UAS that SENT the provisional owes it. On the caller-facing
/// leg that is this stack — the `RSeq` alice saw is its own mint — so an `RAck`
/// naming a number it never sent is answered here and never relayed.
///
/// Relaying it instead would put the decision on the callee, whose sequence
/// space is independent (§3, errata 4600): a number meaningless to alice can be
/// meaningful to bob, and he would acknowledge a provisional she was never
/// shown.
#[tokio::test]
async fn a_prack_naming_an_rseq_this_face_never_sent_takes_481() {
    /// Neither the number alice was shown nor bob's own, so a relayed copy is
    /// unmistakable on his side.
    const STRANGER: u32 = 999_999;

    let h = Harness::with_transit_delay("b2bua-prack-unmatched-rack", 0);
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
    reliable_183(&mut uas).await;

    let p183 = call.expect(183).await;
    let shown = rseq_of(&p183);
    assert_ne!(shown, STRANGER, "the fixture's stranger must not collide with the mint");

    // Alice PRACKs a number she was never shown, under the INVITE's own CSeq —
    // everything else about the request is well formed.
    //
    // IN THE EARLY DIALOG, which is the only place a PRACK belongs: RFC 3262 §3
    // scopes the machinery to the dialog-creating INVITE, and §5 puts the PRACK
    // in the early dialog the reliable provisional CREATED. The call is not
    // answered yet, and the assertion below pins the addressing rather than
    // leaving it to the order of these lines.
    let (mut bad, sent) = call
        .send_request(sip_message::generators::InDialogMethod::Prack)
        .with_rack(&format!("{STRANGER} {} INVITE", p183.cseq().seq()))
        .try_send_with_request()
        .await
        .expect("the PRACK goes out");
    assert_eq!(
        sent.to().tag(),
        p183.to().tag(),
        "the PRACK rides the EARLY dialog the reliable provisional opened (RFC 3262 §5)",
    );
    bad.expect(481).await;

    // And bob never saw it: his FIRST PRACK is the good one, naming his own
    // sequence. Had the stranger been relayed, this would read 999999.
    let mut good = call.try_prack(&p183).await.expect("alice PRACKs what she was shown");
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        BOB_RSEQ,
        "only the PRACK naming a provisional this face really sent is relayed",
    );
    bob_prack.respond(200, "OK").await;
    good.expect(200).await;

    // The call still completes: a refused PRACK ends nothing.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

/// A callee repeat of a reliable provisional the caller has ALREADY been shown
/// is ABSORBED, not relayed: RFC 3262 §4 has the b-face UAC discard a
/// retransmission of a received reliable provisional, and that sentence carries
/// no clause about what the caller has done yet. The caller's own copies are
/// this stack's §3 ladder to pace (`prack_reliable_ladder.rs`), so a relayed
/// repeat would put the CALLEE's clock on her wire.
///
/// The invariant this fixture was written for is unchanged and still shows: the
/// repeat RECALLS the a-facing number rather than minting a second, which is
/// what lets alice's single PRACK translate back onto the callee's own `RSeq`.
#[tokio::test(start_paused = true)]
async fn a_pre_prack_callee_repeat_is_absorbed_and_keeps_its_a_facing_number() {
    let h = Harness::with_transit_delay("b2bua-prack-retransmit", 0);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;
    let alice_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();

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

    // Bob repeats it BEFORE any PRACK — his §3 rung, on his own leg.
    reliable_183(&mut uas).await;

    // One PRACK answers the one provisional alice was shown, and it names the
    // number the repeat recalled rather than a second mint.
    let mut prack = call.try_prack(&first).await.expect("alice PRACKs the reliable 183");
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        BOB_RSEQ,
        "the recalled a-facing number still translates back onto the callee's sequence",
    );
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;

    // The vantage is the wire: a relayed repeat is byte-identical to the first,
    // so alice's client transaction would absorb it and `expect` would never
    // show it. Counted up to her PRACK, so no §3 rung of ours can be mistaken
    // for the relayed repeat.
    let entries = report.entries();
    let pracked_at = entries
        .iter()
        .find(|e| e.from == alice_addr && e.to == b2bua.addr && e.raw.starts_with(b"PRACK "))
        .expect("alice PRACKed")
        .sent_ms;
    let shown = entries
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == alice_addr)
        .filter(|e| e.raw.starts_with(b"SIP/2.0 183 ") && e.sent_ms <= pracked_at)
        .count();
    assert_eq!(
        shown, 1,
        "RFC 3262 §4: the callee's repeat is discarded on the b face — alice saw {shown} copies \
         of the provisional before her PRACK",
    );
}

/// A reliable provisional the caller has already PRACKed is RETIRED: a callee
/// repeat of it is absorbed, never relayed. RFC 3262 §3 makes the a-facing UAS
/// cease retransmissions once the matching PRACK is received, and §4 makes the
/// b-facing UAC discard a retransmission (same dialog ID, CSeq, RSeq) outright
/// — so post-PRACK the caller sees nothing, and her next INVITE response is
/// the final. The repeating callee itself violates §3; provoking that is the
/// point of the fixture, and the peer's fault gates nothing here.
#[tokio::test]
async fn a_prack_retired_reliable_provisional_is_not_relayed_again() {
    let h = Harness::with_transit_delay("b2bua-prack-retired", 0);
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5077).start(&h, "b2bua", "127.0.0.1:5087").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // The full acknowledgement round trip: 183 shown, PRACKed, PRACK answered.
    reliable_183(&mut uas).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    // Bob repeats the acknowledged provisional (his §3 violation), then
    // answers. Alice's next INVITE response MUST be the 200: a relayed repeat
    // would arrive first and read 183 here.
    reliable_183(&mut uas).await;
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

/// RFC 3262 §7.2 spells `RAck` `<RSeq> <CSeq-num> <CSeq-method>`, and a PRACK
/// acknowledges a provisional only where ALL THREE tokens name it. The `RSeq`
/// alice really was shown, carried under a `CSeq` number no INVITE of hers ever
/// bore, names nothing — and the face that sent the provisional owes that 481
/// (§4), the same as for an `RSeq` it never sent.
///
/// Relaying it is worse than passing a mismatch on: `relay_request` REWRITES the
/// CSeq token to the b-leg INVITE's own on the way out, so a wrong-CSeq `RAck`
/// leaves this stack fully valid and the callee retires a provisional against an
/// acknowledgement the caller never made.
#[tokio::test]
async fn a_prack_naming_a_shown_rseq_under_a_wrong_cseq_takes_481() {
    let h = Harness::with_transit_delay("b2bua-prack-wrong-rack-cseq", 0);
    // Alice names an INVITE she never opened: her own §7.2 offence, and the one
    // this fixture exists to provoke. Every B2BUA bind stays gated.
    h.waive(
        WaiverScope::rule(
            "rack-without-known-invite",
            "alice deliberately PRACKs under a CSeq number no INVITE of hers carried \
             (RFC 3262 §7.2) — the caller misbehaviour this test exists to refuse",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5501").await;
    let bob = h.agent("bob", "127.0.0.1:5511").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5511).start(&h, "b2bua", "127.0.0.1:5521").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_183(&mut uas).await;

    let p183 = call.expect(183).await;
    let shown = rseq_of(&p183);

    // The RSeq half is this run's own — only the CSeq half is a stranger, so
    // nothing but §7.2's second token can be what draws the 481.
    let wrong_cseq = p183.cseq().seq() + 100;
    let (mut bad, sent) = call
        .send_request(sip_message::generators::InDialogMethod::Prack)
        .with_rack(&format!("{shown} {wrong_cseq} INVITE"))
        .try_send_with_request()
        .await
        .expect("the PRACK goes out");
    assert_eq!(
        sent.to().tag(),
        p183.to().tag(),
        "the PRACK rides the EARLY dialog the reliable provisional opened (RFC 3262 §5)",
    );
    bad.expect(481).await;

    // Alice then PRACKs correctly and bob answers that one.
    let mut good = call.try_prack(&p183).await.expect("alice PRACKs what she was shown");
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        BOB_RSEQ,
        "the relayed PRACK translates back onto the sequence the callee stated",
    );
    bob_prack.respond(200, "OK").await;
    good.expect(200).await;

    // The call still completes: a refused PRACK ends nothing.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // ONE PRACK crossed onto the b leg — the good one. Counting is the only
    // proof available: the relay rewrites the CSeq token and translates the
    // RSeq, so a relayed stray would reach bob byte-identical to this one.
    let bob_addr: SocketAddr = "127.0.0.1:5511".parse().unwrap();
    let relayed = h
        .wire_entries()
        .into_iter()
        .filter(|e| e.to == bob_addr && e.raw.starts_with(b"PRACK "))
        .count();
    assert_eq!(relayed, 1, "the callee saw {relayed} PRACKs — the laundered stray is one of them");

    let _report = h.finish().await;
}

/// RFC 3262 §7.2 makes `RAck` MANDATORY in a PRACK: without it the request names
/// no provisional at all, so there is nothing to match and nothing to translate.
/// This stack is the UAS that sent the provisional, so the malformed request
/// dies here with `400 Bad Request` (RFC 3261 §21.4.1) rather than being handed
/// to the callee, who would answer it against HIS sequence space.
#[tokio::test]
async fn a_prack_carrying_no_rack_takes_400() {
    let h = Harness::with_transit_delay("b2bua-prack-no-rack", 0);
    let alice = h.agent("alice", "127.0.0.1:5502").await;
    let bob = h.agent("bob", "127.0.0.1:5512").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5512).start(&h, "b2bua", "127.0.0.1:5522").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_183(&mut uas).await;
    let p183 = call.expect(183).await;

    let (mut bad, sent) = call
        .send_request(sip_message::generators::InDialogMethod::Prack)
        .try_send_with_request()
        .await
        .expect("the PRACK goes out");
    assert_eq!(
        sent.raw(HeaderName::RAck).next(),
        None,
        "the fixture's PRACK carries no RAck at all — the header §7.2 makes mandatory",
    );
    bad.expect(400).await;

    // Bob's FIRST PRACK is alice's well-formed one: the headerless request was
    // answered here, never handed on.
    let mut good = call.try_prack(&p183).await.expect("alice PRACKs what she was shown");
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        BOB_RSEQ,
        "only a PRACK that names a provisional is relayed",
    );
    bob_prack.respond(200, "OK").await;
    good.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

/// A SECOND PRACK naming a provisional this face really showed is relayed,
/// translated, exactly like the first — the entry outlives its acknowledgement.
///
/// RFC 3262 §4 owes a `481` to a PRACK matching nothing this face SENT; a
/// re-PRACK matches something it did. The number is this stack's own mint, the
/// early dialog is the one it was shown in, and `RAck` translates back onto the
/// callee's sequence as exactly as the first time — so the callee, who owns the
/// provisional, answers for it. Refusing it here would answer for him on a
/// number that is real.
///
/// A genuine PRACK retransmission never reaches this path (same branch — the
/// server transaction re-passes its retained answer, RFC 3261 §17.2.2). What
/// arrives on a fresh branch is a new transaction, and it is relayed.
#[tokio::test]
async fn a_re_prack_of_an_acknowledged_provisional_is_still_relayed() {
    let h = Harness::with_transit_delay("b2bua-prack-re-prack", 0);
    let alice = h.agent("alice", "127.0.0.1:5504").await;
    let bob = h.agent("bob", "127.0.0.1:5514").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5514).start(&h, "b2bua", "127.0.0.1:5524").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_183(&mut uas).await;
    let p183 = call.expect(183).await;

    // The provisional is PRACKed and retired: its a-facing ladder is over.
    let mut first = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    first.expect(200).await;

    // Alice PRACKs it AGAIN on a fresh transaction. The retired entry still
    // translates, so bob sees his own number a second time and answers.
    let mut again = call.try_prack(&p183).await.expect("alice re-PRACKs what she was shown");
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck").rseq(),
        BOB_RSEQ,
        "the acknowledged entry still translates onto the sequence the callee stated",
    );
    bob_prack.respond(200, "OK").await;
    again.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
