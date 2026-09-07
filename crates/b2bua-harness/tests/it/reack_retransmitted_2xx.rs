//! RFC 3261 §13.2.2.4 — the B2BUA MUST **re-ACK a retransmitted 2xx** whose
//! first ACK was lost. The ACK for a 2xx is a UAC-**core** responsibility (a
//! separate transaction), and the answerer re-sends its 2xx end-to-end until
//! ACKed (up to its Timer H ≈ 32 s). So when the B2BUA's ACK to a callee is lost,
//! the callee retransmits its 200 and the B2BUA MUST re-emit the ACK on the SAME
//! client transaction — reusing the first ACK's Via branch + the INVITE CSeq.
//!
//! Without this, a single lost ACK strands the callee's INVITE server txn: the
//! confirmed, bridged call is never fully reaped (leak) or times out late — a
//! genuine SUT bug under real-network packet loss (a confirmed call dropping is
//! always genuine, per `docs/testing/ha-acceptance.md`). This is the b-leg twin
//! of the a-leg `unacked-2xx-retransmit` (which retransmits the B2BUA's *own* 2xx
//! to a silent caller); here the B2BUA is the ACKing party.
//!
//! The scenario establishes a call, then bob (the callee) retransmits its 200 as
//! though the relayed ACK never arrived. The RFC-correct B2BUA re-ACKs — a second
//! ACK to bob, reusing the first ACK's Via branch (a *fresh* branch would mint a
//! new transaction and never quiesce bob) — and the call still reaps cleanly. The
//! harness inbox dedups a same-`(Call-ID, branch, method)` retransmit, so the
//! re-ACK is asserted on the recorded trace (not a second `receive`). This is the
//! default-lane functional gate the slow-lane loadgen loss-soak mirrors
//! end-to-end (the realign/reroute leak the soak reproduces rides the same re-ACK
//! machinery: `AckLeg` → `ack_b_leg` → `ack_branch` → `re-ack-retransmitted-2xx`).

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const BOB_ADDR: &str = "127.0.0.1:5071";
const MASKED_BOB_ADDR: &str = "127.0.0.1:5775";
const ALICE_FP_ADDR: &str = "127.0.0.1:5766";
const FP_BOB_ADDR: &str = "127.0.0.1:5776";

#[tokio::test(start_paused = true)]
async fn retransmitted_2xx_is_re_acked_on_the_same_branch() {
    let h = Harness::new("b2bua-reack-2xx");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", BOB_ADDR).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071).start(&h, "b2bua", "127.0.0.1:5081").await;

    // ── Establish: INVITE → 180 → 200 → ACK, bridged over two dialogs ─────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // alice ACKs; the B2BUA relays the ACK to bob. This FIRST ACK's Via branch is
    // retained on the b-leg dialog (`ack_branch`) for the §13.2.2.4 re-ACK.
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── bob never saw the ACK: it retransmits its 2xx (2xx-until-ACK) ─────────
    // The b-leg INVITE client txn was deleted on the first 200, so this arrives at
    // the core as an unmatched INVITE 2xx on the now-confirmed leg. The RFC-correct
    // B2BUA re-ACKs on the SAME transaction so bob's server txn quiesces.
    uas.respond(200, "OK").with_sdp(ANSWER).await;

    // Pump the paused clock in 100 ms chunks (the transit-hop delay) to deliver
    // retransmit → SUT → re-ACK → bob, draining bob's raw inbox each step (the
    // inbox dedups the re-ACK for `receive` — same Call-ID/branch/method as the
    // first ACK — but the datagram is delivered and recorded). This is the primary
    // gate: WITHOUT the fix no re-ACK is emitted and this stays 0.
    let mut re_acks = 0;
    for _ in 0..10 {
        h.advance(Duration::from_millis(100)).await;
        re_acks += bob.drain().await;
        if re_acks >= 1 {
            break;
        }
    }
    assert_eq!(re_acks, 1, "the SUT MUST re-ACK the retransmitted 2xx (got {re_acks} datagrams)");
    // The re-ACK is a repeat the peer provoked, not an outbound request of its
    // own: counted once as a `trigger` repeat of an ACK, and nowhere else.
    assert_eq!(b2bua.metrics().retransmits_total("trigger", "ACK", None), 1);
    assert_eq!(b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200)), 0, "no ladder ran");

    // ── Teardown: clean BYE both ways; the confirmed call reaps (no leak) ─────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    // §13.2.2.4: exactly two ACKs reached bob (the relayed initial + the re-ACK),
    // and both carry the SAME top-Via branch — a fresh branch would have minted a
    // new client transaction and never quiesced bob's server txn.
    let bob_addr: SocketAddr = BOB_ADDR.parse().unwrap();
    let ack_branches: Vec<String> = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(b"ACK "))
        .filter_map(|e| top_via_branch(&e.raw))
        .collect();
    assert_eq!(
        ack_branches.len(),
        2,
        "the SUT sent two ACKs to bob (relayed initial + §13.2.2.4 re-ACK): {ack_branches:?}",
    );
    assert_eq!(
        ack_branches[0], ack_branches[1],
        "RFC 3261 §13.2.2.4: the re-ACK reuses the first ACK's Via branch",
    );
}

/// The `branch=` of the topmost Via in a raw request (the first `branch=` on the
/// wire is the top Via's — Vias are serialized top-first).
fn top_via_branch(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    let start = s.find("branch=")? + "branch=".len();
    let rest = &s[start..];
    let end = rest
        .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// The same §13.2.2.4 obligation under an **18x-masking** strategy. The
/// `relayFirst18x` machine stays armed for the life of the call, so its
/// `force-tag-consistency` rule sees every b-leg INVITE 2xx — a retransmission
/// included. It must leave the retransmission to CORE `re-ack-retransmitted-2xx`
/// (which owns the confirmed leg) instead of re-running the answer path, or bob's
/// ACK is never repaired and his 2xx ladder runs to Timer H on a bridged call.
#[tokio::test(start_paused = true)]
async fn retransmitted_2xx_is_re_acked_under_18x_masking() {
    let h = Harness::new("b2bua-reack-2xx-masked");
    let alice = h.agent("alice", "127.0.0.1:5765").await;
    let bob = h.agent("bob", MASKED_BOB_ADDR).await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5775, RelayFirst18xStrategy::KeepSdp)
        .start(&h, "b2bua", "127.0.0.1:5785")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // bob retransmits its 2xx on the now-confirmed leg, as a callee whose ACK was
    // lost does. The script takes the ACK first so the b-leg holds the branch the
    // re-ACK must reuse; the peer-side deviation is that bob repeats anyway.
    uas.respond(200, "OK").with_sdp(ANSWER).await;

    let mut re_acks = 0;
    for _ in 0..10 {
        h.advance(Duration::from_millis(100)).await;
        re_acks += bob.drain().await;
        if re_acks >= 1 {
            break;
        }
    }
    assert_eq!(re_acks, 1, "the SUT MUST re-ACK the retransmitted 2xx (got {re_acks} datagrams)");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    let bob_addr: SocketAddr = MASKED_BOB_ADDR.parse().unwrap();
    let ack_branches: Vec<String> = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(b"ACK "))
        .filter_map(|e| top_via_branch(&e.raw))
        .collect();
    assert_eq!(
        ack_branches.len(),
        2,
        "the SUT sent two ACKs to bob (relayed initial + §13.2.2.4 re-ACK): {ack_branches:?}",
    );
    assert_eq!(
        ack_branches[0], ack_branches[1],
        "RFC 3261 §13.2.2.4: the re-ACK reuses the first ACK's Via branch",
    );

    // The masked call is answered ONCE: a re-run of the answer path would relay a
    // second 200 to alice off the retransmission.
    let alice_addr: SocketAddr = "127.0.0.1:5765".parse().unwrap();
    let finals = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 200 "))
        .filter(|e| String::from_utf8_lossy(&e.raw).contains("CSeq: 1 INVITE"))
        .count();
    assert_eq!(finals, 1, "alice saw exactly one 200 OK for her INVITE (got {finals})");
}

/// The fake-prack arm of the same gate. `force-tag-consistency` declines at the
/// matcher, before its strategy branch, so a retransmitted 2xx never re-stages
/// the cached SDP into a second relayed 200 — alice keeps the ONE answer she was
/// given, body and all.
#[tokio::test(start_paused = true)]
async fn retransmitted_2xx_under_fake_prack_does_not_restage_the_answer() {
    let h = Harness::new("b2bua-reack-2xx-fakeprack");
    let alice = h.agent("alice", ALICE_FP_ADDR).await;
    let bob = h.agent("bob", FP_BOB_ADDR).await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5776, RelayFirst18xStrategy::FakePrack)
        .start(&h, "b2bua", "127.0.0.1:5786")
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    // Reliable 183 → bare 180 to alice, PRACKed by the B2BUA; bob's SDP is cached
    // and injected into the 200, which is the staging this test guards.
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    uas.respond(200, "OK").await;
    let answer = call.expect(200).await;
    assert!(!answer.body().is_empty(), "alice's 200 carries the cached callee SDP");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    uas.respond(200, "OK").await;

    let mut re_acks = 0;
    for _ in 0..10 {
        h.advance(Duration::from_millis(100)).await;
        re_acks += bob.drain().await;
        if re_acks >= 1 {
            break;
        }
    }
    assert_eq!(re_acks, 1, "the SUT MUST re-ACK the retransmitted 2xx (got {re_acks} datagrams)");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    // One answer to alice, not two: a re-run of the answer path would re-stage the
    // cached SDP and relay a second — RFC-legal as a 2xx retransmit, so the count
    // is the only thing that can catch it.
    let alice_addr: SocketAddr = ALICE_FP_ADDR.parse().unwrap();
    let finals = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 200 "))
        .filter(|e| String::from_utf8_lossy(&e.raw).contains("CSeq: 1 INVITE"))
        .count();
    assert_eq!(finals, 1, "alice saw exactly one 200 OK for her INVITE (got {finals})");
}

const FORK_BOB_ADDR: &str = "127.0.0.1:5072";

/// **A repeat is the same dialog's 2xx, not merely the same CSeq.** Every fork of
/// one INVITE answers on that INVITE's CSeq (RFC 3261 §12.1.2), so a losing
/// fork's late 2xx reaches this leg carrying a tag the B2BUA never confirmed.
/// Taking it for a retransmission would ACK it off the SURVIVING dialog — the
/// winner's To-tag, at the winner's remote target — an ACK addressed to a dialog
/// that never sent this response, which quiesces nobody.
///
/// Driven on a leg the BYE has already Terminated, the state
/// `re-ack-retransmitted-2xx` was widened to (RFC 5407 §2), because that is where
/// a straggling fork answer most plausibly lands.
#[tokio::test(start_paused = true)]
async fn a_foreign_tagged_2xx_is_not_a_retransmission() {
    let h = Harness::new("b2bua-reack-2xx-fork-loser");
    // The straggler is the deliberate non-compliance: it answers under a tag
    // nobody confirmed and then neither gets ACKed nor cleans its own dialog up.
    // Refusing to adopt it is the behaviour under test.
    h.allow_violation(
        "unacked-2xx-not-cleared",
        "the scripted fork-loser 2xx is the corner case: an answer under an unconfirmed tag, which the SUT must not adopt",
    );
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", FORK_BOB_ADDR).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5072).start(&h, "b2bua", "127.0.0.1:5082").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // ── bob hangs up: his leg goes Terminated while the call is still live,
    //    the window the widened rule reaches. alice holds her BYE unanswered so
    //    the record is not reaped out from under the straggler. ───────────────
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    let mut alice_bye = alice.receive("BYE").await;
    bob.drain().await;

    // ── a straggler answers on the same INVITE CSeq under a tag of its own ───
    uas.respond(200, "OK").with_to_tag("z-fork-loser").with_sdp(ANSWER).await;
    let mut stragglers = 0;
    for _ in 0..6 {
        h.advance(Duration::from_millis(100)).await;
        stragglers += bob.drain().await;
    }
    assert_eq!(stragglers, 0, "a 2xx under an unconfirmed tag draws no ACK (got {stragglers} datagrams)");
    assert_eq!(b2bua.metrics().retransmits_total("trigger", "ACK", None), 0, "nothing was repeated");

    alice_bye.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    let bob_addr: SocketAddr = FORK_BOB_ADDR.parse().unwrap();
    let acks = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(b"ACK "))
        .count();
    assert_eq!(acks, 1, "one ACK reached bob — the relayed initial one, and nothing off the straggler");
}
