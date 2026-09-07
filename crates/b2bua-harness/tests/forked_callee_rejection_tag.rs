//! **Which caller-facing To-tag a forked callee's FINAL rides** — the split this
//! B2BUA makes, and the one the reference platform makes.
//!
//! A b-leg that rings several early dialogs gives the caller one early dialog per
//! callee fork (RFC 3261 §12.1.2), each under its own a-facing tag. When a
//! NON-FIRST fork ends the transaction the two halves part company:
//!
//! - a **2xx** rides the ANSWERING fork's a-tag — §13.2.2.4 has that 2xx
//!   establish the dialog, so the caller's ACK and every in-dialog request must
//!   address it;
//! - a **non-2xx final** rides the caller leg's PRIMARY (first fork's) a-tag,
//!   whichever fork rejected. A final response ends the one INVITE transaction
//!   and establishes nothing, so the tag it carries names no dialog the caller
//!   keeps; the leg's primary is what the a-leg server transaction answers under.
//!
//! Both halves are asserted here, in one file, because the pair is the contract:
//! widening `relay_response.rs`'s per-fork tag map from `(100..300)` to
//! `(100..700)` in good faith inverts the second half and nothing else notices.
//! A survey of 10 188 corpus flows finds the reference platform doing exactly
//! this split — 11/11 non-2xx finals from a non-first fork on the primary tag,
//! 37/37 2xx on the answering fork's — with no counter-example either way.

use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;
use sip_message::types::SipResponse;
use std::net::SocketAddr;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER_F2: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

fn rseq_of(resp: &SipResponse) -> u32 {
    resp.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value()
}

/// The To-tag on the ACK the caller's transaction layer put on the wire for
/// `cseq` (§17.1.1.3) — read off the recording, since the auto-ACK is the txn
/// layer's own and never surfaces to the body.
fn caller_ack_to_tag(h: &Harness, from: SocketAddr, to: SocketAddr, cseq: u32) -> String {
    h.wire_entries()
        .into_iter()
        .find(|e| {
            e.from == from
                && e.to == to
                && e.raw.starts_with(b"ACK ")
                && sip_message::sniff::cseq_number(&e.raw) == Some(cseq)
        })
        .map(|e| sip_message::sniff::to_tag(&e.raw))
        .expect("the caller's txn layer ACKed the non-2xx final")
}

/// The rejected call left one CDR carrying a reject, and the B2BUA holds nothing.
async fn assert_rejected_and_reaped(h: &Harness, b2bua: &B2buaSut) {
    settle_until(|| !b2bua.cdr_records().is_empty() && b2bua.active_calls() == 0).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the rejected call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Reject), "reject event: {kinds:?}");
    let _ = h;
    b2bua.assert_fully_reaped();
}

/// Two plain 180s under two callee tags, so the caller holds two early dialogs;
/// fork 2 then rejects the transaction. The caller-facing 503 rides fork 1's
/// a-tag — the leg's primary — and the caller's own ACK follows it there
/// (§17.1.1.3: the ACK copies the To of the response it acknowledges), while the
/// B2BUA's b-facing ACK stays in fork 2, where the 503 was actually generated.
#[tokio::test]
async fn a_non_first_fork_s_rejection_rides_the_primary_caller_tag() {
    const ALICE: &str = "127.0.0.1:6041";
    const B2BUA: &str = "127.0.0.1:6043";
    let h = Harness::with_transit_delay("b2bua-forked-rejection-primary-tag", 1);
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", "127.0.0.1:6042").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6042).start(&h, "b2bua", B2BUA).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // ── two callee forks ring, so the caller holds two early dialogs ─────────
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let p1 = call.expect(180).await;
    let fork1_atag = p1.to().tag().expect("fork1 a-facing tag").to_string();

    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let p2 = call.expect(180).await;
    let fork2_atag = p2.to().tag().expect("fork2 a-facing tag").to_string();
    assert_ne!(fork1_atag, fork2_atag, "each callee fork maps to a distinct a-facing tag");

    // ── fork 2 ends the transaction ──────────────────────────────────────────
    uas.respond(503, "Service Unavailable").with_to_tag("bobfork2").await;

    let b_ack = bob.receive("ACK").await; // the b2bua completes bob's reject txn (§17.1.1.3)
    assert_eq!(
        b_ack.request().to().tag(),
        Some("bobfork2"),
        "the B2BUA's b-facing ACK acknowledges the response where it was generated",
    );

    let rejected = call.expect(503).await;
    assert_eq!(
        rejected.to().tag(),
        Some(fork1_atag.as_str()),
        "a non-2xx final rides the caller leg's PRIMARY tag, not the rejecting fork's",
    );
    assert_eq!(
        caller_ack_to_tag(&h, ALICE.parse().unwrap(), B2BUA.parse().unwrap(), 1),
        fork1_atag,
        "the caller's ACK copies the To of the final it acknowledges (§17.1.1.3)",
    );

    assert_rejected_and_reaped(&h, &b2bua).await;
    alice.drain().await;
    bob.drain().await;
    let _report = h.finish().await;
}

/// The same split with both forks ringing RELIABLY and each PRACKed in its own
/// early dialog (RFC 3262 §4, errata 4603: a ladder per early dialog). Spending
/// a PRACK in each fork moves neither the tag the rejection rides nor the
/// per-fork sequences 245/242 pinned.
#[tokio::test]
async fn a_non_first_reliable_fork_s_rejection_rides_the_primary_caller_tag() {
    const ALICE: &str = "127.0.0.1:6044";
    const B2BUA: &str = "127.0.0.1:6046";
    let h = Harness::with_transit_delay("b2bua-forked-reliable-rejection-primary-tag", 1);
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", "127.0.0.1:6045").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6045).start(&h, "b2bua", B2BUA).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // ── fork 1 rings reliably and the caller PRACKs it in its own dialog ─────
    uas.respond(180, "Ringing").with_to_tag("bobfork1").reliable(1).await;
    let p1 = call.expect(180).await;
    let fork1_atag = p1.to().tag().expect("fork1 a-facing tag").to_string();
    let mut prack1 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork1_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p1)))
        .send()
        .await;
    let mut prack1_at_bob = bob.receive("PRACK").await;
    assert_eq!(prack1_at_bob.request().to().tag(), Some("bobfork1"), "PRACK rides fork 1");
    assert_eq!(prack1_at_bob.request().cseq().seq(), 2, "fork 1's own INVITE + 1");
    prack1_at_bob.respond(200, "OK").await;
    prack1.expect(200).await;

    // ── fork 2 rings reliably on ITS OWN ladder and is PRACKed in its own ────
    uas.respond(180, "Ringing").with_to_tag("bobfork2").reliable(1).await;
    let p2 = call.expect(180).await;
    let fork2_atag = p2.to().tag().expect("fork2 a-facing tag").to_string();
    assert_ne!(fork1_atag, fork2_atag, "each callee fork maps to a distinct a-facing tag");
    let mut prack2 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork2_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p2)))
        .send()
        .await;
    let mut prack2_at_bob = bob.receive("PRACK").await;
    assert_eq!(prack2_at_bob.request().to().tag(), Some("bobfork2"), "PRACK rides fork 2");
    assert_eq!(
        prack2_at_bob.request().cseq().seq(),
        2,
        "fork 2 numbers from its own INVITE — the number fork 1's PRACK also carried",
    );
    prack2_at_bob.respond(200, "OK").await;
    prack2.expect(200).await;

    // ── fork 2 rejects ───────────────────────────────────────────────────────
    uas.respond(503, "Service Unavailable").with_to_tag("bobfork2").await;
    let b_ack = bob.receive("ACK").await;
    assert_eq!(b_ack.request().to().tag(), Some("bobfork2"), "the b-facing ACK stays in fork 2");

    let rejected = call.expect(503).await;
    assert_eq!(
        rejected.to().tag(),
        Some(fork1_atag.as_str()),
        "a reliably-rung fork's rejection still rides the caller leg's PRIMARY tag",
    );
    assert_eq!(
        caller_ack_to_tag(&h, ALICE.parse().unwrap(), B2BUA.parse().unwrap(), 1),
        fork1_atag,
        "the caller's ACK copies the To of the final it acknowledges (§17.1.1.3)",
    );

    assert_rejected_and_reaped(&h, &b2bua).await;
    alice.drain().await;
    bob.drain().await;
    let _report = h.finish().await;
}

/// The 2xx twin, and the whole point of keeping it beside the two above: the
/// SAME non-first fork that carried the rejection there carries the ANSWER here,
/// and the caller-facing 200 rides that fork's OWN a-tag (§13.2.2.4) — the
/// dialog the caller then ACKs, re-INVITEs and BYEs. The two halves must not be
/// able to drift apart unnoticed.
#[tokio::test]
async fn a_non_first_fork_s_answer_rides_its_own_caller_tag() {
    let h = Harness::with_transit_delay("b2bua-forked-answer-own-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:6047").await;
    let bob = h.agent("bob", "127.0.0.1:6048").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6048).start(&h, "b2bua", "127.0.0.1:6049").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let p1 = call.expect(180).await;
    let fork1_atag = p1.to().tag().expect("fork1 a-facing tag").to_string();

    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let p2 = call.expect(180).await;
    let fork2_atag = p2.to().tag().expect("fork2 a-facing tag").to_string();
    assert_ne!(fork1_atag, fork2_atag, "each callee fork maps to a distinct a-facing tag");

    uas.respond(200, "OK").with_to_tag("bobfork2").with_sdp(ANSWER_F2).await;
    let ok = call.expect(200).await;
    assert_eq!(
        ok.to().tag(),
        Some(fork2_atag.as_str()),
        "a 2xx rides the ANSWERING fork's a-tag — the dialog it establishes",
    );

    let mut dialog = call.ack().await;
    let ack_at_bob = bob.receive("ACK").await;
    assert_eq!(ack_at_bob.request().to().tag(), Some("bobfork2"), "the ACK rides the answering fork");

    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(bye_at_bob.request().to().tag(), Some("bobfork2"), "the BYE rides the answering fork");
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
