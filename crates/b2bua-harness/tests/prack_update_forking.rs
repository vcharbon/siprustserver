//! Multiple early dialogs (forking) with **PRACK mixed with UPDATE**, then the
//! call is answered on the *second* fork and confirmed in-dialog traffic is
//! exercised end-to-end (port idea: `prack-forking.ts` ⊕ RFC 3311 UPDATE).
//!
//! Bob (standing in for a forking proxy upstream of him) answers Alice's INVITE
//! — which carries the offer — with **two** reliable 183s on distinct callee
//! To-tags: two independent early dialogs on one b-leg, each supplying its own
//! SDP answer (RFC 3262 §5 + RFC 3264 §4: one offer/answer per early dialog). The
//! B2BUA maps each callee fork-tag to its own a-facing tag, so Alice sees two
//! early dialogs, PRACKs each (RFC 3262), then issues an **UPDATE** on fork 2 to
//! re-negotiate media *before answer* (RFC 3311 §5.1) — the canonical "change the
//! early media / hold before 200" case. Bob answers the UPDATE with a fresh SDP
//! answer.
//!
//! Bob finally answers the INVITE on **fork 2** (deliberately *not* the first
//! fork — the surviving dialog must be whichever one wins, not the first seen).
//! The other early dialog is abandoned. We then drive confirmed in-dialog
//! requests on the established dialog — re-INVITE (+ACK), in-dialog UPDATE, INFO,
//! then BYE — and assert each is relayed to Bob carrying the *fork-2* callee tag.
//!
//! ```text
//!   INVITE(offer)
//!     → 183(fork1, Require:100rel RSeq:1, answer1) → PRACK(fork1) → 200(PRACK)
//!     → 183(fork2, Require:100rel RSeq:1, answer2) → PRACK(fork2) → 200(PRACK)
//!     → UPDATE(fork2, re-offer) → 200(UPDATE, re-answer)
//!     → 200(INVITE, fork2) → ACK
//!     → re-INVITE(offer) → 200 → ACK → UPDATE → 200 → INFO → 200 → BYE → 200
//! ```
//!
//! Exercises the multi-early-dialog relay (`add-tag-mapping`/`find-by-a-tag`),
//! per-dialog CSeq sequences (RFC 3261 §12.2.1.1), the RAck CSeq rewrite
//! (RFC 3262 §7.2), the `relay-update` path during the *early* state, and that
//! the confirmed dialog is the *winning* fork — all checked against the recorded
//! callflow by the harness RFC audit at `finish()`.

use b2bua_harness::B2buaSut;
use scenario_harness::{Harness, RunReport};
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;
use std::path::Path;

// Alice's initial offer, two codecs so the re-offer can narrow it.
const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
// Each fork's answer (distinct media port per fork — independent early media).
const ANSWER_F1: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER_F2: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
// Alice re-offers on fork 2 (hold: sendonly, single codec) before answer.
const REOFFER_HOLD: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const REANSWER_HELD: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
// Confirmed-dialog re-INVITE (resume: sendrecv) and its answer.
const REINVITE_RESUME: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const REINVITE_ANSWER: &str = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

#[tokio::test]
async fn prack_update_forking_answer_on_second_fork() {
    let h = Harness::with_transit_delay("b2bua-prack-update-forking", 1);
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5077).start(&h, "b2bua", "127.0.0.1:5087").await;

    // Alice INVITEs with the offer in the INVITE, advertising 100rel support.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(!uas.request().body.is_empty(), "offer relayed to bob on the INVITE");

    // ── Fork 1: reliable 183 (callee tag `bobfork1`) with answer 1 ───────────
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork1")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER_F1)
        .await;
    let p1 = call.expect(183).await;
    assert_eq!(
        get_header(&p1.headers, "require").as_deref(),
        Some("100rel"),
        "fork1 Require:100rel relayed to alice",
    );
    let fork1_atag = p1.to.tag.clone().expect("fork1 a-facing tag");

    // Alice PRACKs fork 1 (addressed to fork1's a-tag).
    let mut prack1 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork1_atag)
        .with_rack("1 1 INVITE")
        .send()
        .await;
    let mut prack1_at_bob = bob.receive("PRACK").await;
    assert_eq!(
        prack1_at_bob.request().to.tag.as_deref(),
        Some("bobfork1"),
        "PRACK for fork1 carries the callee fork1 tag",
    );
    prack1_at_bob.respond(200, "OK").await;
    prack1.expect(200).await;

    // ── Fork 2: reliable 183 (callee tag `bobfork2`) with answer 2 ───────────
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork2")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER_F2)
        .await;
    let p2 = call.expect(183).await;
    let fork2_atag = p2.to.tag.clone().expect("fork2 a-facing tag");
    assert_ne!(fork1_atag, fork2_atag, "each callee fork maps to a distinct a-facing tag");

    // Alice PRACKs fork 2 — its own dialog, first PRACK also at INVITE_CSeq+1.
    let mut prack2 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork2_atag)
        .with_rack("1 1 INVITE")
        .send()
        .await;
    let mut prack2_at_bob = bob.receive("PRACK").await;
    assert_eq!(
        prack2_at_bob.request().to.tag.as_deref(),
        Some("bobfork2"),
        "PRACK for fork2 carries the callee fork2 tag",
    );
    prack2_at_bob.respond(200, "OK").await;
    prack2.expect(200).await;

    // ── UPDATE on fork 2 (RFC 3311): re-negotiate media *before* answer ──────
    // Alice puts the early media on hold with a re-offer on the fork-2 dialog.
    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&fork2_atag)
        .with_sdp(REOFFER_HOLD)
        .send()
        .await;
    let mut update_at_bob = bob.receive("UPDATE").await;
    assert_eq!(
        update_at_bob.request().to.tag.as_deref(),
        Some("bobfork2"),
        "UPDATE is relayed on the fork2 early dialog",
    );
    // Media fidelity (RFC 3264): the re-offer's media direction (hold) is
    // relayed to bob verbatim, so RTP renegotiates end-to-end correctly.
    let bob_offer = String::from_utf8_lossy(&update_at_bob.request().body).to_string();
    assert!(bob_offer.contains("a=sendonly"), "hold re-offer direction relayed to bob: {bob_offer}");
    update_at_bob.respond(200, "OK").with_sdp(REANSWER_HELD).await;
    let upd_ok = update.expect(200).await;
    let alice_answer = String::from_utf8_lossy(&upd_ok.body).to_string();
    assert!(
        alice_answer.contains("a=recvonly"),
        "held answer direction relayed back to alice: {alice_answer}",
    );

    // ── Bob answers the INVITE on fork 2 (the *winning* dialog) ──────────────
    uas.respond(200, "OK").with_to_tag("bobfork2").await;
    let ok = call.expect(200).await;
    assert_eq!(
        ok.to.tag.as_deref(),
        Some(fork2_atag.as_str()),
        "the confirmed dialog is fork 2's a-facing tag",
    );
    let mut dialog = call.ack().await;
    let ack_at_bob = bob.receive("ACK").await;
    assert_eq!(
        ack_at_bob.request().to.tag.as_deref(),
        Some("bobfork2"),
        "ACK is sent on the fork2 dialog",
    );
    // The caller's confirmed dialog adopts the *winning* fork's a-facing tag —
    // the ACK toward the B2BUA carries it, not the first fork's (RFC 3261 §12.1).
    assert_eq!(
        get_header(&ack_at_bob.request().headers, "cseq").map(|c| c.trim()),
        Some("1 ACK"),
        "the 2xx ACK reuses the INVITE CSeq (1), not the post-UPDATE local CSeq (RFC 3261 §13.2.2.4)",
    );

    // ── Confirmed in-dialog traffic on the established (fork 2) dialog ────────
    // re-INVITE (resume from hold) + ACK.
    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(REINVITE_RESUME)).await;
    let mut reinvite_at_bob = bob.receive("INVITE").await;
    assert_eq!(
        reinvite_at_bob.request().to.tag.as_deref(),
        Some("bobfork2"),
        "re-INVITE stays on the fork2 confirmed dialog",
    );
    let resume_offer = String::from_utf8_lossy(&reinvite_at_bob.request().body).to_string();
    assert!(
        resume_offer.contains("a=sendrecv"),
        "resume (un-hold) re-offer direction relayed to bob: {resume_offer}",
    );
    reinvite_at_bob.respond(200, "OK").with_sdp(REINVITE_ANSWER).await;
    reinvite.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await;

    // In-dialog UPDATE on the confirmed dialog.
    let mut cupdate = dialog.request(InDialogMethod::Update, Some(REOFFER_HOLD)).await;
    let mut cupdate_at_bob = bob.receive("UPDATE").await;
    assert_eq!(
        cupdate_at_bob.request().to.tag.as_deref(),
        Some("bobfork2"),
        "confirmed UPDATE stays on the fork2 dialog",
    );
    cupdate_at_bob.respond(200, "OK").with_sdp(REANSWER_HELD).await;
    cupdate.expect(200).await;

    // In-dialog INFO.
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;

    // ── Teardown ─────────────────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(
        bye_at_bob.request().to.tag.as_deref(),
        Some("bobfork2"),
        "BYE tears down the fork2 dialog",
    );
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    finish_with_report(h).await;
}

// ── fake-prack forking (GAP-P7-1): per-To-tag PRACK targeting + SDP cache ────
//
// Under `relayFirst18xTo180` strategy `fake-prack` the B2BUA itself PRACKs each
// reliable 1xx and caches each fork's SDP answer, keyed strictly on the
// `(leg, callee To-tag)` early dialog. The regression these two tests pin:
// pre-fix the machine registered only the RELAYED fork's early dialog, so a
// second fork's suppressed 183 fell back to the FIRST dialog — its PRACK went
// out carrying fork 1's To-tag (mis-targeted RAck) and its cache OVERWROTE
// fork 1's answer. Now each distinct callee To-tag gets its own early dialog,
// PRACK, and SDP cache; whichever fork wins the 200, alice's answer is THAT
// fork's cached SDP.

/// The RAck header is `<rseq> <invite-cseq> INVITE` (RFC 3262 §7.2).
fn rack_of(headers: &[sip_message::SipHeader]) -> String {
    get_header(headers, "rack").unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drive the shared fake-prack forking prelude: alice's reliable-offer INVITE,
/// two reliable 183s on distinct callee To-tags (fork 1 relayed to alice as a
/// bare 180, fork 2 suppressed), and the B2BUA's per-fork PRACKs — asserting
/// each PRACK targets its own fork's dialog (To-tag + RAck). Returns
/// `(uas, a-facing 180 tag)` with both forks PRACKed, ready for a 200.
async fn fake_prack_fork_prelude(
    call: &mut scenario_harness::ClientInvite,
    bob: &scenario_harness::Agent,
) -> (scenario_harness::ServerTxn, String) {
    let mut uas = bob.receive("INVITE").await;

    // Fork 1: reliable 183 (tag `bobfork1`) with answer 1 → alice sees a bare
    // 180; the B2BUA PRACKs fork 1.
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork1")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER_F1)
        .await;
    let p180 = call.expect(180).await;
    assert!(p180.body.is_empty(), "bare 180 has no body");
    let a_tag = p180.to.tag.clone().expect("bare 180 has a To-tag");
    let mut prack1 = bob.receive("PRACK").await;
    assert_eq!(
        prack1.request().to.tag.as_deref(),
        Some("bobfork1"),
        "fork1's PRACK targets fork1's early dialog",
    );
    assert_eq!(rack_of(&prack1.request().headers), "1 1 INVITE", "fork1 RAck");
    prack1.respond(200, "OK").await;

    // Fork 2: reliable 183 (tag `bobfork2`) with answer 2 — suppressed for
    // alice (FIRST policy), but the B2BUA must still PRACK **fork 2's** dialog
    // (pre-fix this PRACK fell back to fork 1's To-tag).
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork2")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER_F2)
        .await;
    let mut prack2 = bob.receive("PRACK").await;
    assert_eq!(
        prack2.request().to.tag.as_deref(),
        Some("bobfork2"),
        "fork2's PRACK targets fork2's early dialog (no first-dialog fallback)",
    );
    assert_eq!(rack_of(&prack2.request().headers), "1 1 INVITE", "fork2 RAck (own dialog CSeq space)");
    prack2.respond(200, "OK").await;

    (uas, a_tag)
}

/// Fake-prack forked b-leg answered on **fork 1**: its cached SDP must survive
/// fork 2's later 183 (pre-fix, fork 2's answer overwrote fork 1's cache and
/// alice's 200 carried the WRONG fork's SDP).
#[tokio::test]
async fn fake_prack_forking_answer_on_first_fork_keeps_its_own_cache() {
    let h = Harness::with_transit_delay("fake-prack-forking-first-fork", 1);
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let b2bua = B2buaSut::route_all_to_with_18x(
        "127.0.0.1",
        5078,
        call::features::RelayFirst18xStrategy::FakePrack,
    )
    .start(&h, "b2bua", "127.0.0.1:5088")
    .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let (mut uas, a_tag) = fake_prack_fork_prelude(&mut call, &bob).await;

    // Bob answers on fork 1 with NO body — alice's 200 must carry **fork 1's**
    // cached 183 SDP, not fork 2's (the overwrite regression).
    uas.respond(200, "OK").with_to_tag("bobfork1").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(&ok.body),
        ANSWER_F1,
        "alice's 200 carries fork 1's own cached SDP (no cross-fork overwrite)",
    );
    assert_eq!(ok.to.tag.as_deref(), Some(a_tag.as_str()), "200 reuses the bare 180's To-tag");

    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to.tag.as_deref(), Some("bobfork1"), "ACK rides fork 1's dialog");

    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(bye_at_bob.request().to.tag.as_deref(), Some("bobfork1"), "BYE rides fork 1's dialog");
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Fake-prack forked b-leg answered on **fork 2** (the suppressed fork): its
/// own early dialog must exist with its own cached SDP (pre-fix fork 2 had no
/// dialog at all — its state lived, wrongly, on fork 1's).
#[tokio::test]
async fn fake_prack_forking_answer_on_second_fork_uses_its_own_cache() {
    let h = Harness::with_transit_delay("fake-prack-forking-second-fork", 1);
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let b2bua = B2buaSut::route_all_to_with_18x(
        "127.0.0.1",
        5079,
        call::features::RelayFirst18xStrategy::FakePrack,
    )
    .start(&h, "b2bua", "127.0.0.1:5089")
    .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let (mut uas, a_tag) = fake_prack_fork_prelude(&mut call, &bob).await;

    // Bob answers on fork 2 with NO body — alice's 200 carries **fork 2's**
    // cached SDP, still under the first bare 180's To-tag (mask continuity even
    // when a suppressed fork wins).
    uas.respond(200, "OK").with_to_tag("bobfork2").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(&ok.body),
        ANSWER_F2,
        "alice's 200 carries fork 2's own cached SDP",
    );
    assert_eq!(
        ok.to.tag.as_deref(),
        Some(a_tag.as_str()),
        "200 reuses the bare 180's To-tag even though the suppressed fork won",
    );

    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to.tag.as_deref(), Some("bobfork2"), "ACK rides fork 2's dialog");

    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(bye_at_bob.request().to.tag.as_deref(), Some("bobfork2"), "BYE rides fork 2's dialog");
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Finish the run (triggers the RFC audit hard-gate over the recorded trace) and
/// render the callflow artifacts under `target/seq-reports/prack-update-forking/`
/// so the `.global.txt` can be reviewed for SIP correctness.
async fn finish_with_report(h: Harness) {
    let report: RunReport = h.finish().await;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports/prack-update-forking");
    let paths = scenario_harness::report::write_all(&report, &dir).expect("write report");
    for p in &paths {
        if p.extension().is_some_and(|e| e == "txt") && p.to_string_lossy().contains("global") {
            eprintln!("callflow: {}", p.display());
        }
    }
}
