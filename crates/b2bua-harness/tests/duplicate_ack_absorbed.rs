//! **One b-leg ACK per 2xx RECEIVED, never one per a-leg ACK.** RFC 3261
//! §13.2.2.4 puts the ACK in the UAC core: the obligation is drawn by the 2xx
//! copies this stack takes off the b-leg, so a caller that ACKs twice for one
//! answer says nothing about the callee's transaction and must draw nothing.
//!
//! The duplicate is legal input either way it arrives — §8.1.1.7 exempts only a
//! CANCEL and a non-2xx ACK from branch uniqueness, so a caller that GENERATES a
//! second ACK carries a fresh branch while one that RE-PASSES the first carries
//! the same. Both are the same obligation, so absorption keys on dialog + CSeq
//! and never on the branch. Ticket 146.
//!
//! Boundary, held by `reack_retransmitted_2xx` / `reack_2xx_before_caller_ack`:
//! absorbing a surplus ACK must not absorb an ACK owed to a repeated 2xx.

use std::net::SocketAddr;
use std::time::Duration;

use call::CdrEventType;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// The ACK `from` put on the wire for `cseq`, as recorded — the datagram a
/// duplicate repeats.
fn recorded_ack(h: &Harness, from: SocketAddr, to: SocketAddr, cseq: u32) -> Vec<u8> {
    h.wire_entries()
        .into_iter()
        .find(|e| {
            e.from == from
                && e.to == to
                && e.raw.starts_with(b"ACK ")
                && sip_message::sniff::cseq_number(&e.raw) == Some(cseq)
        })
        .map(|e| e.raw)
        .expect("the caller ACKed the 2xx")
}

/// The same datagram on a fresh top-Via branch — the copy a caller GENERATES
/// rather than re-passes.
fn on_fresh_branch(raw: &[u8], branch: &str) -> Vec<u8> {
    let s = String::from_utf8(raw.to_vec()).expect("an ACK this harness wrote is UTF-8");
    let at = s.find("branch=").expect("a top Via branch") + "branch=".len();
    let end = at + s[at..].find(|c: char| c == ';' || c == ',' || c.is_whitespace()).unwrap_or(s.len() - at);
    format!("{}{branch}{}", &s[..at], &s[end..]).into_bytes()
}

/// Every ACK the SUT put on the callee's socket, in order.
fn acks_to_callee(h: &Harness, b2bua: SocketAddr, callee: SocketAddr) -> Vec<Vec<u8>> {
    h.wire_entries()
        .into_iter()
        .filter(|e| e.from == b2bua && e.to == callee && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw)
        .collect()
}

/// One answered call, and the caller ACKs twice with DISTINCT branches — the
/// captured shape (ticket 145, flows leg 5 msgs 4/5). The callee answered once,
/// so it is owed exactly one ACK.
#[tokio::test(start_paused = true)]
async fn a_duplicate_ack_on_a_fresh_branch_draws_no_second_b_leg_ack() {
    const BOB: &str = "127.0.0.1:5071";
    let h = Harness::new("b2bua-duplicate-ack-fresh-branch");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", BOB).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071).start(&h, "b2bua", "127.0.0.1:5081").await;
    let bob_addr: SocketAddr = BOB.parse().unwrap();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── the duplicate: same dialog, same CSeq, a fresh branch ────────────────
    let first = recorded_ack(&h, alice.addr(), b2bua.addr, invite_cseq);
    let dup = on_fresh_branch(&first, "z9hG4bK-alice-dup-fresh");
    assert_ne!(dup, first, "the duplicate must differ from the ACK it repeats");
    alice.try_send_datagram(&dup, b2bua.addr).await.unwrap();
    h.advance(Duration::from_millis(600)).await;

    let acks = acks_to_callee(&h, b2bua.addr, bob_addr);
    assert_eq!(
        acks.len(),
        1,
        "one ACK per 2xx RECEIVED (RFC 3261 §13.2.2.4): bob answered once, so a second \
         caller ACK draws nothing"
    );

    // ── the call is untouched by the absorption: clean teardown, one CDR ─────
    bob.drain().await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR per call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Answer), "answer: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Bye), "bye: {kinds:?}");
    assert_eq!(cdrs[0].b_legs.len(), 1, "one b-leg");

    let _report = h.finish().await;
}

/// The same duplicate with the SAME branch — a plain re-pass. The harness inbox
/// dedups on identity AND bytes, but it guards a harness UA's `receive`, not the
/// SUT's socket, so this datagram does reach the B2BUA; the count is read off
/// the recorded wire regardless.
#[tokio::test(start_paused = true)]
async fn a_duplicate_ack_on_the_same_branch_draws_no_second_b_leg_ack() {
    const BOB: &str = "127.0.0.1:5072";
    let h = Harness::new("b2bua-duplicate-ack-same-branch");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", BOB).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5072).start(&h, "b2bua", "127.0.0.1:5082").await;
    let bob_addr: SocketAddr = BOB.parse().unwrap();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let dup = recorded_ack(&h, alice.addr(), b2bua.addr, invite_cseq);
    alice.try_send_datagram(&dup, b2bua.addr).await.unwrap();
    h.advance(Duration::from_millis(600)).await;

    let acks = acks_to_callee(&h, b2bua.addr, bob_addr);
    assert_eq!(
        acks.len(),
        1,
        "a re-passed ACK is the same obligation as the one it repeats: bob answered once"
    );

    bob.drain().await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// The re-INVITE twin: a duplicate ACK for a renegotiation's 2xx, arriving after
/// the obligation is discharged. It must draw no third ACK and must leave the
/// re-INVITE watchdogs alone — the call still tears down clean.
#[tokio::test(start_paused = true)]
async fn a_duplicate_reinvite_ack_draws_no_third_b_leg_ack() {
    const BOB: &str = "127.0.0.1:5073";
    let h = Harness::new("b2bua-duplicate-ack-reinvite");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", BOB).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;
    let bob_addr: SocketAddr = BOB.parse().unwrap();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── renegotiate, answered once, ACKed once ───────────────────────────────
    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // ── the duplicate, after the obligation is discharged ────────────────────
    let dup = on_fresh_branch(
        &recorded_ack(&h, alice.addr(), b2bua.addr, reinvite_cseq),
        "z9hG4bK-alice-dup-reinvite",
    );
    alice.try_send_datagram(&dup, b2bua.addr).await.unwrap();
    h.advance(Duration::from_millis(600)).await;

    let acks = acks_to_callee(&h, b2bua.addr, bob_addr);
    assert_eq!(
        acks.len(),
        2,
        "two 2xx were received on the b-leg (the answer and the renegotiation), so two \
         ACKs are owed and no more"
    );

    bob.drain().await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
