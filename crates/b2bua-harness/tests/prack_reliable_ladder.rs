//! RFC 3262 §3 — the **caller-facing** reliable-provisional retransmission
//! ladder this stack owes as the a-face UAS, and nothing else. PRACK *relay*
//! (the number, the `RAck` translation, the retired-provisional absorption)
//! lives in `prack.rs`; forking lives in `prack_forking.rs`.
//!
//! §3 attaches the obligation to whoever assigns the sequence, and the number
//! the caller sees is this stack's own (`assign_a_rseq`). So the ladder under
//! the caller's copies is ours: it starts at T1 and doubles, it stops the
//! moment her PRACK names the number, and it is bounded by 64·T1. What the
//! callee does on its own leg pays for none of it — a callee that never repeats
//! leaves the caller no second copy at all unless we send one, and a callee
//! that repeats off-schedule must not set the caller's pace.
//!
//! **The wire is the vantage.** A re-send of a reliable provisional is the same
//! datagram — same `RSeq`, same CSeq, same Via branch — so the caller's client
//! transaction absorbs it as the retransmission it is and `expect` never sees a
//! second one. Every assertion here therefore reads the recording.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallFailureResponse, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{advance, settle_until, B2buaSut, ADVANCE_STEP_MS};
use scenario_harness::{Harness, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Bob's own sequence — deliberately far from any number this stack mints, so
/// "the copies carry OUR number" is provable rather than coincidental.
const BOB_RSEQ: u32 = 4711;

/// The rerouted callee's own sequence — distinct from [`BOB_RSEQ`] for the same
/// reason: whose numbering the caller sees must be provable, not coincidental.
const BOB2_RSEQ: u32 = 9182;

/// RFC 3261 T1. §3's ladder starts here and doubles; 64·T1 bounds it.
const T1_MS: u64 = 500;

/// The caller-facing copies of one reliable provisional, as gaps between
/// consecutive emissions: §3's "exponential backoff … starting at T1 and
/// doubling" is a rule about INTERVALS, so the copies fall 500 ms, then 1 s,
/// then 2 s after the one before — 500 ms / 1.5 s / 3.5 s after the first.
const LADDER_GAPS_MS: [u64; 3] = [T1_MS, 2 * T1_MS, 4 * T1_MS];

/// RFC 3262 §3's give-up bound on an unacknowledged reliable provisional.
const GIVE_UP_MS: u64 = 64 * T1_MS;

/// The advance step: small enough that every deadline in play here (a 500 ms
/// rung, a 408 ms callee repeat) is landed on exactly.
const STEP_MS: u64 = ADVANCE_STEP_MS;

fn reliable_183(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(ANSWER)
}

/// A reliable provisional carrying NO answer — the offer/answer exchange stays
/// on the INVITE/200, so no §3 "delay the 2xx until the SDP is PRACKed"
/// obligation gates a test whose subject is the ladder alone.
fn reliable_180(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
}

/// The `RSeq` a raw response names, if any.
fn rseq_in(raw: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(raw).ok()?;
    s.split("\r\n")
        .filter(|l| l.len() > 5 && l[..5].eq_ignore_ascii_case("RSeq:"))
        .find_map(|l| l[5..].trim().parse().ok())
}

/// The rerouted callee's reliable ring, on its own sequence.
fn reliable_180_bob2(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB2_RSEQ.to_string())
}

/// The `To` tag a raw response names, if any — the a-facing early dialog it
/// belongs to, which is the scope RFC 3262 §3's ban is written in.
fn to_tag_in(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    let to = s.split("\r\n").find(|l| {
        l.len() > 3 && (l[..3].eq_ignore_ascii_case("To:") || l[..2].eq_ignore_ascii_case("t:"))
    })?;
    to.split(';').find_map(|p| p.trim().strip_prefix("tag=").map(|t| t.trim().to_string()))
}

/// Every copy of the `status` response the SUT put on `to`'s wire, in send
/// order, as `(sent_ms, RSeq)`.
fn copies_to(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
    status: u16,
) -> Vec<(u64, u32)> {
    let head = format!("SIP/2.0 {status} ");
    let mut out: Vec<(u64, u32)> = entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .map(|e| (e.sent_ms, rseq_in(&e.raw).unwrap_or(0)))
        .collect();
    out.sort_by_key(|(ms, _)| *ms);
    out
}

/// The gaps between consecutive copies (ms).
fn gaps(copies: &[(u64, u32)]) -> Vec<u64> {
    copies.windows(2).map(|w| w[1].0 - w[0].0).collect()
}

/// Assert `copies` fall on §3's ladder: each gap is its nominal interval, plus
/// at most one advance step of scheduling slack — the simulated pipeline's, not
/// the ladder's. The floor is the load-bearing half: a rung is never EARLY, and
/// that is exactly what separates our clock from a callee's.
fn assert_ladder(copies: &[(u64, u32)], nominal: &[u64]) {
    let measured = gaps(copies);
    assert_eq!(
        measured.len(),
        nominal.len(),
        "expected {} rungs, got {measured:?} from {copies:?}",
        nominal.len(),
    );
    for (i, (got, want)) in measured.iter().zip(nominal).enumerate() {
        assert!(
            (*want..=*want + STEP_MS).contains(got),
            "rung {} lands {got} ms after the copy before it, not the {want} ms RFC 3262 §3 owes \
             (T1, doubling): {copies:?}",
            i + 1,
        );
    }
}

/// Every copy names the number the caller was first shown — a ladder rung is a
/// RETRANSMISSION, never a fresh reliable provisional she would owe a second
/// PRACK for (RFC 3262 §4, errata 4603).
fn assert_one_number(copies: &[(u64, u32)]) {
    let first = copies[0].1;
    assert!(
        copies.iter().all(|(_, r)| *r == first),
        "every ladder rung retransmits the SAME a-facing RSeq: {copies:?}",
    );
}

/// **The defect, in one assertion.** The callee sends its reliable provisional
/// exactly ONCE and never repeats it; the caller withholds her PRACK. Nothing
/// on the b face can put a second copy on the caller's wire, so every copy she
/// receives after the first is this stack's §3 ladder — and today she receives
/// none at all, because no ladder is ever armed.
#[tokio::test(start_paused = true)]
async fn the_caller_facing_ladder_runs_on_our_own_clock() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder", 0);
    let alice = h.agent("alice", "127.0.0.1:5401").await;
    let bob = h.agent("bob", "127.0.0.1:5411").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5411).start(&h, "b2bua", "127.0.0.1:5421").await;
    let alice_addr: SocketAddr = "127.0.0.1:5401".parse().unwrap();
    let bob_addr: SocketAddr = "127.0.0.1:5411".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // ── ONE reliable provisional from the callee, and never another ──────────
    reliable_183(&mut uas).await;
    let p183 = call.expect(183).await;

    // Alice withholds her PRACK across three rungs (500 ms + 1 s + 2 s = 3.5 s).
    advance(3_600).await;
    alice.drain().await;
    // Each rung is counted as it leaves, under the §3 ladder and the
    // provisional it repeats — the 183 to her INVITE.
    let counted = || b2bua.metrics().retransmits_total("reliable-provisional", "INVITE", Some(183));
    assert_eq!(
        counted(),
        3,
        "b2bua_retransmits_total{{reliable-provisional,INVITE,183}} climbs with the rungs"
    );

    // ── her PRACK retires the number and the ladder stops ────────────────────
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    // The callee sent exactly one — so nothing but our ladder can account for
    // the caller's further copies.
    let from_bob = copies_to(&entries, bob_addr, b2bua.addr, 183);
    assert_eq!(from_bob.len(), 1, "the fixture's callee repeats nothing: {from_bob:?}");

    let seen = copies_to(&entries, b2bua.addr, alice_addr, 183);
    assert_eq!(
        seen.len(),
        1 + LADDER_GAPS_MS.len(),
        "RFC 3262 §3: the a-face UAS retransmits its reliable provisional until PRACKed — \
         alice got {} copies of a provisional the callee sent once (a silent wire is the defect)",
        seen.len(),
    );
    assert_ladder(&seen, &LADDER_GAPS_MS);
    assert_one_number(&seen);
    assert_eq!(counted(), 3, "the PRACK stopped the ladder: nothing more was counted");
    assert_eq!(
        b2bua.metrics().repeat_give_ups_total("prack-of"),
        0,
        "a PRACKed ladder never gives up"
    );
}

/// **The two halves compose.** The callee repeats its reliable provisional on
/// its own, non-RFC cadence (408 ms — `capture_213117`'s measured gap) while
/// the caller withholds her PRACK. The repeat is absorbed on the b face (§4:
/// a retransmission of a received reliable provisional is discarded), and the
/// caller's copies stay on OUR ladder: their count is ours, and not one of them
/// falls on the callee's clock.
#[tokio::test(start_paused = true)]
async fn the_callees_own_pacing_never_reaches_the_caller() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-offbeat", 0);
    let alice = h.agent("alice", "127.0.0.1:5402").await;
    let bob = h.agent("bob", "127.0.0.1:5412").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5412).start(&h, "b2bua", "127.0.0.1:5422").await;
    let alice_addr: SocketAddr = "127.0.0.1:5402".parse().unwrap();

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

    // The callee's own rung, 408 ms in — inside our first 500 ms interval, so a
    // relayed copy would be unmistakable in the gaps below.
    advance(408).await;
    reliable_183(&mut uas).await;
    advance(3_200).await;
    alice.drain().await;

    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;

    let seen = copies_to(&report.entries(), b2bua.addr, alice_addr, 183);
    assert_eq!(
        seen.len(),
        1 + LADDER_GAPS_MS.len(),
        "the COUNT is ours: the callee's 408 ms repeat is absorbed on the b face (RFC 3262 §4) \
         and is not a copy alice may see — {seen:?}",
    );
    assert_ladder(&seen, &LADDER_GAPS_MS);
    assert_one_number(&seen);
}

/// RFC 3262 §3 bounds the ladder at 64·T1 and then ANSWERS the silence: the
/// retransmissions cease, and the caller who never PRACKed is rejected — §3's
/// "SHOULD reject the original request with a 5xx". On a B2BUA that sentence is
/// a call teardown, so the pending b-leg is CANCELled with her and the call
/// reaps; the alternative, riding to the 150 s setup deadline, rings a real
/// callee for two more minutes on behalf of a caller who is gone.
#[tokio::test(start_paused = true)]
async fn the_ladder_gives_up_at_64_t1() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-giveup", 0);
    // Alice deliberately never PRACKs — that withholding IS the fixture, and it
    // is the only way to reach the give-up bound. Every other bind stays gated.
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice deliberately never PRACKs, so the a-face ladder runs to its 64·T1 bound \
             (RFC 3262 §3) — the caller's silence is this test's subject",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5403").await;
    let bob = h.agent("bob", "127.0.0.1:5413").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5413).start(&h, "b2bua", "127.0.0.1:5423").await;
    let alice_addr: SocketAddr = "127.0.0.1:5403".parse().unwrap();

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
    let _ = p183;

    // Every rung inside the bound has fired (the last falls at 63·T1). Drain
    // them, so what alice reads next is the give-up's own answer and not a
    // retransmission queued behind it.
    advance(GIVE_UP_MS - T1_MS + 2 * STEP_MS).await;
    alice.drain().await;

    // §3's reject reaches the caller at 64·T1, and the b-leg is torn down with
    // her — on a B2BUA the reject is a call teardown, not a timer's last line.
    advance(T1_MS + 2 * STEP_MS).await;
    call.expect(504).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    // The give-up is counted once, under the obligation alice left
    // undischarged; every rung before it was counted as it left.
    assert_eq!(
        b2bua.metrics().repeat_give_ups_total("prack-of"),
        1,
        "one give-up: her PRACK never came"
    );
    assert_eq!(b2bua.metrics().repeat_give_ups_total("ack-of-2xx"), 0);
    let report = h.finish().await;
    let entries = report.entries();

    let seen = copies_to(&entries, b2bua.addr, alice_addr, 183);
    let first = seen[0].0;
    assert!(seen.len() >= 5, "a 64·T1 ladder is a real ladder, not one rung: {seen:?}",);
    assert_eq!(
        b2bua.metrics().retransmits_total("reliable-provisional", "INVITE", Some(183)) as usize,
        seen.len() - 1,
        "the counter is the rungs on alice's wire — the first copy is not a repeat",
    );
    assert_ladder(&seen[..1 + LADDER_GAPS_MS.len()], &LADDER_GAPS_MS);
    assert!(
        seen.iter().all(|(ms, _)| ms - first <= GIVE_UP_MS),
        "RFC 3262 §3: no rung falls beyond 64·T1 ({GIVE_UP_MS} ms) after the first emission — {seen:?}",
    );
    assert_one_number(&seen);

    // The deadline is the FIRST EMISSION's, not the last rung's: the reject
    // lands one T1 after the ladder's final copy, and never before the bound.
    let rejected_at = copies_to(&entries, b2bua.addr, alice_addr, 504)
        .first()
        .expect("RFC 3262 §3: the unacknowledged provisional is rejected, not merely abandoned")
        .0;
    assert!(
        (GIVE_UP_MS..GIVE_UP_MS + STEP_MS).contains(&(rejected_at - first)),
        "the 5xx lands at 64·T1 ({GIVE_UP_MS} ms) after the first emission, not {} ms",
        rejected_at - first,
    );
}

/// The bound answers SILENCE, not slowness. A PRACK landing after the last rung
/// but inside 64·T1 retires the number, and the give-up deadline is scrubbed
/// with the ladder it belongs to — the call rings on and answers normally. This
/// is the guard the reject needs: a deadline that fired on a call that had
/// already been acknowledged would cut live calls short at 32 s, and the corpus
/// carries 43 calls that ring past that bound with a reliable provisional out.
#[tokio::test(start_paused = true)]
async fn a_prack_inside_the_bound_is_not_rejected() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-late-prack", 0);
    let alice = h.agent("alice", "127.0.0.1:5407").await;
    let bob = h.agent("bob", "127.0.0.1:5417").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5417).start(&h, "b2bua", "127.0.0.1:5427").await;
    let alice_addr: SocketAddr = "127.0.0.1:5407".parse().unwrap();

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

    // Every rung inside the bound has fired (the last falls at 63·T1); alice
    // answers on that last rung, with the deadline one T1 away.
    advance(GIVE_UP_MS - T1_MS + STEP_MS).await;
    alice.drain().await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the last rung");
    let mut bob_prack = bob.receive("PRACK").await;
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;

    // Well past the deadline the PRACK retired.
    advance(GIVE_UP_MS).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    assert!(
        copies_to(&entries, b2bua.addr, alice_addr, 504).is_empty(),
        "a PRACKed provisional owes no §3 reject — the deadline dies with the ladder",
    );
    assert!(
        !copies_to(&entries, b2bua.addr, alice_addr, 200).is_empty(),
        "the call answers normally after a PRACK inside the bound",
    );
}

/// The final response ends the provisional, so it ends the ladder: a rung
/// emitted after the caller has been answered would retransmit a 1xx into a
/// confirmed dialog (RFC 3261 §12/§13 — and RFC 3262's own
/// no-new-reliable-1xx-after-final).
#[tokio::test(start_paused = true)]
async fn the_ladder_ceases_at_the_final_response() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-final", 0);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice deliberately never PRACKs — the subject is the ladder's cancellation at the \
             final response, which a PRACK would reach first",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5404").await;
    let bob = h.agent("bob", "127.0.0.1:5414").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5414).start(&h, "b2bua", "127.0.0.1:5424").await;
    let alice_addr: SocketAddr = "127.0.0.1:5404".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    reliable_180(&mut uas).await;
    call.expect(180).await;

    // One rung fires, then the callee answers.
    advance(700).await;
    alice.drain().await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Long past the next rung's due time — a live ladder would have fired here.
    advance(3_000).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    let rings = copies_to(&entries, b2bua.addr, alice_addr, 180);
    assert_ladder(&rings, &[T1_MS]);
    let answered_at =
        copies_to(&entries, b2bua.addr, alice_addr, 200).first().expect("alice was answered").0;
    assert!(
        rings.iter().all(|(ms, _)| *ms < answered_at),
        "the ladder is cancelled by the final response — no 180 after the 200 at {answered_at} ms: {rings:?}",
    );
}

/// A cancelled setup ends the provisional too: the caller who hung up must not
/// keep receiving rungs for a call she abandoned.
#[tokio::test(start_paused = true)]
async fn the_ladder_ceases_on_the_callers_cancel() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-cancel", 0);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice CANCELs instead of PRACKing — the subject is the ladder's cancellation on an \
             abandoned setup",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5405").await;
    let bob = h.agent("bob", "127.0.0.1:5415").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5415).start(&h, "b2bua", "127.0.0.1:5425").await;
    let alice_addr: SocketAddr = "127.0.0.1:5405".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    reliable_180(&mut uas).await;
    call.expect(180).await;

    advance(700).await;
    alice.drain().await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;

    // Two rungs' worth of quiet after the cancellation.
    advance(3_000).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    let rings = copies_to(&entries, b2bua.addr, alice_addr, 180);
    let released_at =
        copies_to(&entries, b2bua.addr, alice_addr, 487).first().expect("alice was released 487").0;
    assert_ladder(&rings, &[T1_MS]);
    assert!(
        rings.iter().all(|(ms, _)| *ms < released_at),
        "the ladder is cancelled with the setup — no 180 after the 487 at {released_at} ms: {rings:?}",
    );
}

/// A ladder belongs to the FORK that raised the provisional, so a fork that
/// fails takes its ladder with it. The call reroutes and lives on; a rung that
/// survived the swap would retransmit a dead leg's early state on our clock,
/// and a PRACK answering it would name a b-leg that no longer exists.
#[tokio::test(start_paused = true)]
async fn the_ladder_dies_with_the_fork_that_raised_it() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-failover", 0);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice never PRACKs the failed fork's ring — the subject is what happens to its \
             ladder when the call reroutes underneath it",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5406").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5416").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5417").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5416);
                // The failover-capable marker: without it a b-leg rejection is
                // relayed to the caller instead of consulting `/call/failure`.
                r.callback_context = Some("ladder-failover".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_req| CallFailureResponse::Route(route_to("127.0.0.1", 5417)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5426").await;
    let alice_addr: SocketAddr = "127.0.0.1:5406".parse().unwrap();
    let bob2_addr: SocketAddr = "127.0.0.1:5417".parse().unwrap();

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // The first fork rings reliably, so a ladder is armed under its number.
    let mut uas1 = bob1.receive("INVITE").await;
    reliable_180(&mut uas1).await;
    call.expect(180).await;
    advance(700).await;
    alice.drain().await;

    // …then fails. The rejection is not relayed: the call reroutes to bob2.
    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;
    let mut uas2 = bob2.receive("INVITE").await;

    // Two rungs' worth of quiet is owed here — the leg that raised the ring is
    // gone.
    advance(3_000).await;
    alice.drain().await;

    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    let rerouted_at = entries
        .iter()
        .find(|e| e.from == b2bua.addr && e.to == bob2_addr && e.raw.starts_with(b"INVITE "))
        .expect("the call rerouted to the second callee")
        .sent_ms;
    let rings = copies_to(&entries, b2bua.addr, alice_addr, 180);
    assert_ladder(&rings, &[T1_MS]);
    assert!(
        rings.iter().all(|(ms, _)| *ms < rerouted_at),
        "the failed fork's ladder is cancelled with its leg — no rung after the reroute at \
         {rerouted_at} ms: {rings:?}",
    );
}

/// **A rerouted leg's ring opens the caller's SECOND early dialog, not a second
/// provisional in her first.** RFC 3262 §3 forbids a second reliable
/// provisional before the first is acknowledged, and that ban is scoped to ONE
/// early dialog (§4, errata 4603/4604): two a-facing dialogs each carrying one
/// outstanding provisional are legal, one dialog carrying two is the violation.
/// In transparent mode the caller's dialog set mirrors the callee's, so the
/// rerouted attempt mints its own a-facing tag — and `assign_a_rseq` seeds that
/// dialog its own `RSeq` space, because §4 has a conformant caller drop
/// everything after a gap.
///
/// **The audit is the assertion**: `no-overlapping-reliable-provisionals` stays
/// GATING here, and before the mint this test fails on it by name — "Sent
/// second reliable 1xx before prior RSeq PRACKed".
#[tokio::test(start_paused = true)]
async fn a_rerouted_ring_opens_its_own_caller_early_dialog() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-reroute-reliable", 0);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice never PRACKs the FAILED fork's ring — that dialog dies with its leg, and the \
             subject is the dialog the reroute opens next",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5408").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5418").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5419").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5418);
                r.callback_context = Some("reroute-reliable".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_req| CallFailureResponse::Route(route_to("127.0.0.1", 5419)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5428").await;
    let alice_addr: SocketAddr = "127.0.0.1:5408".parse().unwrap();

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // Attempt 1 rings reliably and is never PRACKed — the caller's first
    // early dialog, left outstanding.
    let mut uas1 = bob1.receive("INVITE").await;
    reliable_180(&mut uas1).await;
    let first = call.expect(180).await;
    let _ = first;
    advance(700).await;
    alice.drain().await;

    // …then fails, and the call reroutes.
    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;
    let mut uas2 = bob2.receive("INVITE").await;

    // Attempt 2 rings reliably too. Under the primary tag this is §3's
    // violation; under its own dialog it is two legal ladders.
    reliable_180_bob2(&mut uas2).await;
    let second = call.expect(180).await;
    // The FORK-ADDRESSED form: a PRACK belongs to the early dialog the reliable
    // 1xx created (RFC 3262 §5), so it is addressed under THAT response's
    // To-tag. `try_prack` reuses the call's own dialog tag and would PRACK the
    // dead attempt's dialog — a 481 bought by the caller behaving wrongly, not
    // by the SUT.
    let (mut prack, _req) = call
        .try_prack_with_request(&second)
        .await
        .expect("alice PRACKs the rerouted ring in its own early dialog");
    // A timeout HERE is the defect, not a flake: collapsed onto the primary
    // a-tag, the PRACK resolves its peer through `find_by_a_tag`, which takes
    // the FIRST mapping — bob1's, the leg the reroute abandoned. The caller's
    // acknowledgement is relayed to a corpse.
    let mut bob_prack = bob2.receive("PRACK").await;
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;

    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    let rings: Vec<(u64, String, u32)> = entries
        .iter()
        .filter(|e| {
            e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 180 ")
        })
        .filter_map(|e| Some((e.sent_ms, to_tag_in(&e.raw)?, rseq_in(&e.raw)?)))
        .collect();
    let attempt1 = rings.first().expect("attempt 1 rang the caller");
    let attempt2 = rings.iter().find(|(_, tag, _)| *tag != attempt1.1).unwrap_or_else(|| {
        panic!(
            "the rerouted ring opens its own caller-facing early dialog — every 180 rode \
                 one To-tag: {rings:?}"
        )
    });
    assert_ne!(
        attempt2.2,
        attempt1.2 + 1,
        "each early dialog carries its OWN randomly-seeded RSeq space (RFC 3262 §4, errata \
         4603) — the second dialog must not continue the first's numbering: {rings:?}",
    );

    // The final ends the transaction and every early dialog it created (RFC
    // 3261 §13.2.2.3), so the ANSWER carries the dialog that was established —
    // the rerouted attempt's, the one alice PRACKed.
    let answered = entries
        .iter()
        .find(|e| e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 200 "))
        .and_then(|e| to_tag_in(&e.raw))
        .expect("alice was answered");
    assert_eq!(
        answered, attempt2.1,
        "the caller's confirmed dialog is the one that answered: {rings:?}",
    );
}

/// The `CSeq` line a raw message carries, if any.
fn cseq_in(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    s.split("\r\n")
        .find(|l| l.len() > 5 && l[..5].eq_ignore_ascii_case("CSeq:"))
        .map(|l| l[5..].trim().to_string())
}

/// A relayed re-INVITE's reliable provisional carries a ladder of its own, and
/// that ladder ends at the re-INVITE's final: a 2xx before any PRACK is what
/// RFC 3262 §3 permits when the provisional carried no session description,
/// and "if the UAS does send a final response when reliable responses are
/// still unacknowledged, it SHOULD NOT continue to retransmit". The call
/// itself, whose setup ended long before, is untouched.
#[tokio::test(start_paused = true)]
async fn the_ladder_of_a_relayed_reinvite_ceases_at_its_own_final() {
    let h = Harness::with_transit_delay("b2bua-prack-ladder-reinvite-final", 0);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice deliberately never PRACKs the re-INVITE's 183 — the subject is the ladder's \
             cancellation at the re-INVITE's final, which a PRACK would reach first",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5109").await;
    let bob = h.agent("bob", "127.0.0.1:5110").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5110).start(&h, "b2bua", "127.0.0.1:5111").await;
    let alice_addr: SocketAddr = "127.0.0.1:5109".parse().unwrap();

    // ── an ordinary call, established without reliability in play ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs offering 100rel; bob answers reliably, NO SDP ──
    let mut reinv = dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .send()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    reliable_180(&mut re_uas).await;
    let p180 = reinv.expect(180).await;
    let reinvite_cseq = p180.cseq().seq();

    // One rung fires with no PRACK in sight, then bob answers the re-INVITE.
    advance(700).await;
    alice.drain().await;
    re_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // Long past the next rung's due time — a live ladder would have fired here.
    advance(3_000).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();

    let rings = copies_to(&entries, b2bua.addr, alice_addr, 180);
    let reinvite_rings: Vec<(u64, u32)> = entries
        .iter()
        .filter(|e| {
            e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 180 ")
        })
        .filter(|e| cseq_in(&e.raw).as_deref() == Some(&format!("{reinvite_cseq} INVITE")))
        .map(|e| (e.sent_ms, rseq_in(&e.raw).unwrap_or(0)))
        .collect();
    assert_eq!(
        reinvite_rings.len() + 1,
        rings.len(),
        "the setup's own 180 is unreliable: {rings:?}"
    );
    assert_ladder(&reinvite_rings, &[T1_MS]);
    assert!(
        reinvite_rings.iter().all(|(_, rseq)| *rseq != BOB_RSEQ && *rseq != 0),
        "every copy carries this stack's number, not bob's: {reinvite_rings:?}",
    );
    let answered_at = entries
        .iter()
        .filter(|e| {
            e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"SIP/2.0 200 ")
        })
        .filter(|e| cseq_in(&e.raw).as_deref() == Some(&format!("{reinvite_cseq} INVITE")))
        .map(|e| e.sent_ms)
        .min()
        .expect("the re-INVITE was answered");
    assert!(
        reinvite_rings.iter().all(|(ms, _)| *ms < answered_at),
        "the ladder is cancelled by the re-INVITE's final — no 180 after the 200 at {answered_at} ms: \
         {reinvite_rings:?}",
    );
}
