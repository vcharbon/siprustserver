//! PRACK forking with delayed offer (port of `tests/scenarios/prack-forking.ts`).
//!
//! Bob (standing in for a forking proxy upstream of him) answers the offerless
//! INVITE with **two** reliable 183s carrying distinct To-tags — two early
//! dialogs on one b-leg. The B2BUA maps each callee fork-tag to its own a-facing
//! tag, so Alice sees two early dialogs and PRACKs each independently (RFC 3262
//! §5 + RFC 3264 §4: one offer/answer per early dialog). Bob finally answers the
//! INVITE on fork 1; the offer/answer there already completed, so the 200 OK and
//! ACK carry no body.
//!
//! ```text
//!   INVITE(no SDP) → 183(fork1, offer) → PRACK(fork1, answer) → 200(PRACK)
//!                  → 183(fork2, offer) → PRACK(fork2, answer) → 200(PRACK)
//!                  → 200(INVITE, fork1) → ACK → BYE → 200(BYE)
//! ```
//!
//! Exercises the B2BUA's multi-early-dialog relay: per-fork tag mapping
//! (`add-tag-mapping`/`find-by-a-tag`), per-dialog CSeq sequences
//! (RFC 3261 §12.2.1.1), and the RAck CSeq rewrite (RFC 3262 §7.2).

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{RAck, RSeq};
use sip_message::types::SipResponse;
use sip_message::Method;

const ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn rseq_of(resp: &SipResponse) -> u32 {
    resp.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value()
}

#[tokio::test]
async fn prack_forking_two_early_dialogs() {
    let h = Harness::with_transit_delay("b2bua-prack-forking", 0);
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    // Alice INVITEs with NO SDP (delayed-offer model), advertising 100rel.
    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(uas.request().body().is_empty(), "delayed offer: no SDP on the INVITE");

    // ── Fork 1: 183 with the callee fork-tag `bobfork1` + offer ──────────────
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork1")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(OFFER)
        .await;
    let p1 = call.expect(183).await;
    let fork1_atag = p1.to().tag().expect("fork1 a-facing tag").to_string();

    // Alice PRACKs fork 1 (answer in the PRACK), addressed to fork1's a-tag.
    let mut prack1 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork1_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p1)))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut prack1_at_bob = bob.receive("PRACK").await;
    assert_eq!(
        prack1_at_bob.request().to().tag(),
        Some("bobfork1"),
        "PRACK for fork1 carries the callee fork1 tag",
    );
    prack1_at_bob.respond(200, "OK").await;
    prack1.expect(200).await;

    // ── Fork 2: 183 with a *different* callee fork-tag `bobfork2` ────────────
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork2")
        .with_header("Require", "100rel")
        .with_header("RSeq", "200")
        .with_sdp(OFFER)
        .await;
    let p2 = call.expect(183).await;
    let fork2_atag = p2.to().tag().expect("fork2 a-facing tag").to_string();
    assert_ne!(fork1_atag, fork2_atag, "each callee fork maps to a distinct a-facing tag");

    let mut prack2 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork2_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p2)))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut prack2_at_bob = bob.receive("PRACK").await;
    assert_eq!(
        prack2_at_bob.request().to().tag(),
        Some("bobfork2"),
        "PRACK for fork2 carries the callee fork2 tag",
    );
    // RAck CSeq token is rewritten to the b-leg INVITE CSeq (RFC 3262 §7.2).
    assert_eq!(
        prack2_at_bob
            .request()
            .header::<RAck>()
            .expect("relayed PRACK keeps an RAck")
            .expect("readable RAck")
            .method(),
        &Method::Invite,
        "the RAck acknowledges the INVITE transaction",
    );
    prack2_at_bob.respond(200, "OK").await;
    prack2.expect(200).await;

    // ── Fork 1 rings again, AFTER fork 2 interleaved ─────────────────────────
    // RFC 3262 §4 (errata 4603/4604) keeps the caller's sequence independently
    // per early dialog, so this must be fork 1's own previous number plus
    // exactly one — a ladder shared with fork 2 would leave a gap here, and a
    // conformant caller drops a provisional whose RSeq is not the next one.
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork1")
        .with_header("Require", "100rel")
        .with_header("RSeq", "2")
        .with_sdp(OFFER)
        .await;
    let p3 = call.expect(183).await;
    assert_eq!(
        p3.to().tag(),
        Some(fork1_atag.as_str()),
        "the second fork-1 provisional stays in fork 1's early dialog",
    );
    assert_eq!(
        rseq_of(&p3),
        rseq_of(&p1) + 1,
        "fork 1's ladder rises by exactly one across the fork-2 interleave",
    );

    let mut prack3 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork1_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p3)))
        .with_sdp(ANSWER)
        .send()
        .await;
    let mut prack3_at_bob = bob.receive("PRACK").await;
    assert_eq!(
        prack3_at_bob
            .request()
            .header::<RAck>()
            .expect("relayed PRACK keeps an RAck")
            .expect("readable RAck")
            .rseq(),
        2,
        "the RAck translates back onto the number fork 1 itself stated",
    );
    prack3_at_bob.respond(200, "OK").await;
    prack3.expect(200).await;

    // ── Bob answers the INVITE on fork 1 (no body — offer/answer done) ───────
    uas.respond(200, "OK").with_to_tag("bobfork1").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── Hangup ───────────────────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
