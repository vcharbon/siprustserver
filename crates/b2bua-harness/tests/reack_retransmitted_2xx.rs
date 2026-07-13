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
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const BOB_ADDR: &str = "127.0.0.1:5071";

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
