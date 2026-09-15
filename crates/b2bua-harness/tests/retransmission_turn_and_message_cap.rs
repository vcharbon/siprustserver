//! A dialog-level retransmission turn of this stack's OWN ladder is not a
//! message the peer sent: a rung of the un-ACKed 2xx ladder (RFC 3261
//! §13.3.1.4) or of the un-PRACKed reliable-provisional ladder (RFC 3262 §3)
//! repeats a retained datagram on this node's own clock and changes nothing a
//! peer, a CDR or a takeover node needs. The per-call runaway-message cap
//! (`max_messages_per_call`) counts in-dialog traffic, so a rung must not
//! count toward it: a deaf caller who lets the ladder run its schedule is not
//! a runaway dialog, and tearing the call down for it turns a lossy path into
//! a dropped call.
//!
//! A repeated INBOUND 2xx is the peer's message: its §13.2.2.4 re-ACK is a
//! quiet repeat too, but the arrival still counts toward the cap — a callee
//! that repeats its 2xx without end is exactly the runaway the cap exists for.
//!
//! Each scenario pins a cap small enough that the rungs alone would cross it
//! and large enough that the call's own signalling never does. The count opens
//! at 1 on the INVITE and every in-dialog turn adds one. What the version
//! vector and the replication stream do on such a turn is not observable on a
//! single node (a `bak`-less call never flushes); the failover-harness twin
//! `retransmission_turn_is_quiet.rs` pins that half.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::TerminationCause;
use scenario_harness::{Harness, RunReport};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The window both ladders run in: every rung due inside it has left, the
/// next one has not, and the give-ups (Timer L, 64·T1) are still ahead.
const LADDER_WINDOW: Duration = Duration::from_millis(16_500);

/// The un-ACKed 2xx ladder's rungs inside [`LADDER_WINDOW`]: T1, doubling,
/// capped at T2 (RFC 3261 §13.3.1.4) — 0.5, 1.5, 3.5, 7.5, 11.5 and 15.5 s
/// after the original; the seventh (19.5 s) is outside.
const RUNGS_2XX: u64 = 6;

/// The cap for the 2xx scenario: the INVITE, the 180 and the 200 stand at 3,
/// the rungs would take it to 9; the ACK and the BYE take the count to 5.
const CAP_2XX: u64 = RUNGS_2XX + 2;

/// The reliable-provisional ladder's rungs inside [`LADDER_WINDOW`]: T1,
/// doubling with no T2 cap (RFC 3262 §3) — 0.5, 1.5, 3.5, 7.5 and 15.5 s
/// after the original; the sixth (31.5 s) is outside.
const RUNGS_PROVISIONAL: u64 = 5;

/// The cap for the provisional scenario: the INVITE and the 183 stand at 2,
/// the rungs would take it to 7; the PRACK, its 200, the 200, the ACK and the
/// BYE take the count to 7 — at the cap, never past it — and the callee's 200
/// to the relayed BYE lands on a `Terminating` call, where the cap no longer
/// reads.
const CAP_PROVISIONAL: u64 = RUNGS_PROVISIONAL + 2;

/// A B2BUA routing every call to `dest_port`, with `max_messages_per_call`
/// pinned at `cap` and the give-up deadlines at their RFC defaults.
async fn b2bua_with_cap(h: &Harness, addr: &str, dest_port: u16, cap: u64) -> B2buaSut {
    let decision =
        Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", dest_port));
    B2buaSut::builder(decision)
        .tune(move |c| {
            c.max_messages_per_call = cap;
        })
        .start(h, "b2bua", addr)
        .await
}

/// The number of BYE requests the SUT put on `to`'s wire.
fn byes_sent(report: &RunReport, sut: SocketAddr, to: SocketAddr) -> usize {
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == to && e.raw.starts_with(b"BYE "))
        .count()
}

/// The un-ACKed 2xx ladder walks [`RUNGS_2XX`] rungs while the caller withholds
/// her ACK, then she ACKs and hangs up: the call is never torn down for the
/// rungs, and the caller's BYE is the only BYE on her wire.
#[tokio::test(start_paused = true)]
async fn a_2xx_ladder_rung_does_not_count_toward_the_message_cap() {
    let h = Harness::new("b2bua-2xx-rung-not-a-message");
    let alice = h.agent("alice", "127.0.0.1:5461").await;
    let bob = h.agent("bob", "127.0.0.1:5471").await;
    let b2bua = b2bua_with_cap(&h, "127.0.0.1:5481", 5471, CAP_2XX).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // ── alice holds her ACK across the whole ladder window ───────────────────
    // The deviation is hers and is the subject: a lossy path between the 2xx
    // and its ACK is what §13.3.1.4's ladder exists for.
    h.advance(LADDER_WINDOW).await;
    let copies = alice.drain().await;
    let counted = || b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200));
    assert_eq!(counted(), RUNGS_2XX, "every rung inside the window left, and none past it");
    assert_eq!(counted(), copies as u64, "one increment per copy alice received");

    // ── her late ACK retires the ladder ──────────────────────────────────────
    // The ACK is the first in-dialog turn after the rungs: a count the rungs
    // inflated is enacted here, so the cap is read once it has been served.
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    h.advance(Duration::from_millis(300)).await;
    assert_eq!(
        b2bua.metrics().message_cap_terminated_total(),
        0,
        "a rung of this stack's own 2xx ladder is not an in-dialog message: the cap must \
         not trip after {RUNGS_2XX} rungs under max_messages_per_call = {CAP_2XX}",
    );

    // ── she hangs up ─────────────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(counted(), RUNGS_2XX, "the ACK stopped the ladder: nothing more was counted");
    assert_eq!(
        b2bua.metrics().repl_quiet_turns_total("own-rung"),
        RUNGS_2XX,
        "every rung was persisted as a quiet turn",
    );
    assert_eq!(b2bua.metrics().repeat_give_ups_total("ack-of-2xx"), 0, "no give-up");

    let records = b2bua.cdr_records();
    assert_eq!(records.len(), 1, "one CDR");
    assert_eq!(
        records[0].termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the caller's BYE ended the call, not the cap",
    );

    let report = h.finish().await;
    assert_eq!(
        byes_sent(&report, b2bua.addr, alice.addr()),
        0,
        "no BYE reached the caller before her own: the SUT never tore the call down",
    );
}

/// The un-PRACKed reliable-provisional ladder walks [`RUNGS_PROVISIONAL`] rungs while the
/// caller withholds her PRACK, then she PRACKs, the call answers and ends: the
/// same rule on the RFC 3262 §3 ladder.
#[tokio::test(start_paused = true)]
async fn a_reliable_provisional_rung_does_not_count_toward_the_message_cap() {
    let h = Harness::new("b2bua-prov-rung-not-a-message");
    let alice = h.agent("alice", "127.0.0.1:5462").await;
    let bob = h.agent("bob", "127.0.0.1:5472").await;
    let b2bua = b2bua_with_cap(&h, "127.0.0.1:5482", 5472, CAP_PROVISIONAL).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    // The provisional carries no answer, so no §3 "delay the 2xx until the SDP
    // is PRACKed" obligation gates the answer once she does PRACK.
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .await;
    let p183 = call.expect(183).await;

    // ── alice holds her PRACK across the whole ladder window ─────────────────
    h.advance(LADDER_WINDOW).await;
    alice.drain().await;
    let counted = || b2bua.metrics().retransmits_total("reliable-provisional", "INVITE", Some(183));
    assert_eq!(counted(), RUNGS_PROVISIONAL, "every rung inside the window left, and none past it");

    // ── her late PRACK retires the number ────────────────────────────────────
    // The PRACK is the first in-dialog turn after the rungs: a count the rungs
    // inflated is enacted here, so the cap is read once it has been served.
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    h.advance(Duration::from_millis(300)).await;
    assert_eq!(
        b2bua.metrics().message_cap_terminated_total(),
        0,
        "a rung of this stack's own reliable-provisional ladder is not an in-dialog \
         message: the cap must not trip after {RUNGS_PROVISIONAL} rungs under \
         max_messages_per_call = {CAP_PROVISIONAL}",
    );
    prack.expect(200).await;

    // ── the call answers and ends ────────────────────────────────────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(
        counted(),
        RUNGS_PROVISIONAL,
        "the PRACK stopped the ladder: nothing more was counted"
    );
    assert_eq!(
        b2bua.metrics().repl_quiet_turns_total("own-rung"),
        RUNGS_PROVISIONAL,
        "every rung was persisted as a quiet turn",
    );
    assert_eq!(b2bua.metrics().repeat_give_ups_total("prack-of"), 0, "no give-up");

    let records = b2bua.cdr_records();
    assert_eq!(records.len(), 1, "one CDR");
    assert_eq!(
        records[0].termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the caller's BYE ended the call, not the cap",
    );

    let report = h.finish().await;
    assert_eq!(
        byes_sent(&report, b2bua.addr, alice.addr()),
        0,
        "no BYE reached the caller before her own: the SUT never tore the call down",
    );
}

/// Repeat bob's 2xx `times` times, pumping the clock after each so the copy,
/// the SUT's re-ACK and the callee's inbox all settle (each hop lands on a
/// later 100 ms chunk); returns how many datagrams reached bob (the re-ACKs,
/// deduplicated for `receive` but counted by `drain`).
async fn repeat_2xx(
    h: &Harness,
    uas: &mut scenario_harness::ServerTxn,
    bob: &scenario_harness::Agent,
    times: usize,
) -> usize {
    let mut reached = 0;
    for _ in 0..times {
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        for _ in 0..8 {
            h.advance(Duration::from_millis(100)).await;
        }
        reached += bob.drain().await;
    }
    reached
}

/// The re-ACK of a repeated inbound 2xx is a quiet repeat on the wire — one
/// `trigger` re-send per copy — but the copy that provoked it is the peer's
/// message and counts: a callee repeating its 2xx past the cap is torn down
/// under `MessageCap`, and the call still ends properly with one CDR.
#[tokio::test(start_paused = true)]
async fn a_repeated_inbound_2xx_counts_toward_the_message_cap() {
    let h = Harness::new("b2bua-repeated-2xx-counts");
    let alice = h.agent("alice", "127.0.0.1:5463").await;
    let bob = h.agent("bob", "127.0.0.1:5473").await;
    let b2bua = b2bua_with_cap(&h, "127.0.0.1:5483", 5473, CAP_2XX).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;

    // ── bob repeats his 2xx as a callee whose ACK was lost does ──────────────
    // The script takes the ACK first so the b-leg holds the branch the re-ACK
    // must reuse; the peer-side deviation is that bob repeats anyway, and keeps
    // repeating past the cap — the runaway the cap exists for. The count stands
    // at four before the repeats (the INVITE, the 180, the 200, the relayed
    // ACK), so the repeat that takes it past `CAP_2XX` is the fifth.
    let repeats_to_trip = (CAP_2XX - 4 + 1) as usize;
    let re_acks = repeat_2xx(&h, &mut uas, &bob, repeats_to_trip - 1).await;
    assert_eq!(
        re_acks,
        repeats_to_trip - 1,
        "RFC 3261 §13.2.2.4: every repeated 2xx under the cap draws its re-ACK",
    );
    assert_eq!(
        b2bua.metrics().retransmits_total("trigger", "ACK", None),
        (repeats_to_trip - 1) as u64,
        "each re-ACK is counted once as a trigger repeat",
    );
    assert_eq!(b2bua.metrics().message_cap_terminated_total(), 0, "under the cap: no teardown");

    repeat_2xx(&h, &mut uas, &bob, 1).await;
    assert_eq!(
        b2bua.metrics().message_cap_terminated_total(),
        1,
        "the repeat that crosses max_messages_per_call = {CAP_2XX} is the peer's message: \
         the cap counts it and tears the runaway call down",
    );

    // ── the cap's teardown is a proper one: BYE both ways, answered ──────────
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let records = b2bua.cdr_records();
    assert_eq!(records.len(), 1, "one CDR");
    assert_eq!(
        records[0].termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::MessageCap),
        "the CDR names the cap as the cause",
    );
    let _report = h.finish().await;
}
