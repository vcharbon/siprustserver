//! **A caller's BYE on an early dialog ends the call** (RFC 3261 §15, §15.1.2).
//! Once a tagged provisional has gone out, the caller holds an early dialog and
//! may BYE it. The UAS answers the BYE 200, ends the dialog, and answers the
//! still-pending INVITE 487 (§15.1.2 recommends it); this stack sends the 200
//! first. The B2BUA CANCELs the ringing callee so it stops ringing, and the
//! record is a caller release.
//!
//! The shapes around that base:
//! - the BYE racing the callee's 2xx (§9.1, §15.1.2): the crossing answer is
//!   confirmed, ACKed and BYE'd on the callee's side, never relayed;
//! - the callee's own early BYE (§15, last paragraph forbids it, the B2BUA
//!   survives it) with the caller's BYE crossing the final it draws;
//! - a reliable provisional (RFC 3262 §3) the caller never PRACKs: her BYE
//!   ends the retransmission ladder with the setup;
//! - a forked callee (§12.1.2): one BYE on one early dialog, one CANCEL
//!   (§9.1, per transaction), one 487 ending every early dialog (§12.3);
//! - the `Reason` the caller states (RFC 3326 §2) rides the CANCEL minted for
//!   the callee (§16.6).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{
    advance, invite_final_statuses, settle_until, stated, stated_by_response, B2buaSut,
};
use call::{CdrEventType, TerminationCause};
use scenario_harness::{Agent, Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// RFC 3261 T1; RFC 3262 §3's ladder starts here and gives up at 64·T1.
const T1_MS: u64 = 500;

/// Start lines of every datagram `agent` received, in arrival order.
fn start_lines(agent: &Agent) -> Vec<String> {
    agent.wire_view().iter().map(|e| e.start_line()).collect()
}

/// The method a raw message's `CSeq` names.
fn cseq_method(raw: &str) -> Option<&str> {
    raw.split("\r\n")
        .find_map(|l| l.strip_prefix("CSeq:"))
        .and_then(|v| v.split_whitespace().nth(1))
}

/// The datagrams `from` put on `to`'s wire whose start line opens with
/// `head`, as `(sent_ms, raw text)` in send order.
fn sent(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
    head: &str,
) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .map(|e| (e.sent_ms, String::from_utf8_lossy(&e.raw).to_string()))
        .collect();
    out.sort_by_key(|(ms, _)| *ms);
    out
}

async fn reaped(b2bua: &B2buaSut) {
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
}

#[tokio::test(start_paused = true)]
async fn caller_bye_on_early_dialog_ends_the_call_and_cancels_the_callee() {
    let h = Harness::new("b2bua-early-dialog-bye");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071)))
            .start(&h, "b2bua", "127.0.0.1:5081")
            .await;

    // ── a ringing call: the 180 carries a To-tag, so alice holds an early dialog ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let early_tag = ringing.to().tag().expect("the 180 carries an early-dialog To-tag").to_string();

    // ── alice BYEs the early dialog (§15): 200 to the BYE, 487 to the INVITE ──
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    bye.expect(200).await;
    let rejected = call.expect(487).await; // auto-ACKed on the INVITE's branch (§17.1.1.3)
    assert_eq!(rejected.to().tag(), Some(early_tag.as_str()), "the 487 ends the early dialog");
    let seen = start_lines(&alice);
    let at = |prefix: &str| seen.iter().position(|l| l.starts_with(prefix));
    assert!(
        at("SIP/2.0 200") < at("SIP/2.0 487"),
        "200 to the BYE before 487 to the INVITE (this stack's order), got {seen:?}"
    );

    // ── the ringing callee is CANCELled and resolves 487 ──
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    // ── fully reaped, recorded as the caller's release ──
    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));

    let _report = h.finish().await;
}

/// The caller's early BYE races the callee's 2xx (RFC 3261 §9.1: a CANCEL
/// that arrives after the final has no effect; §15.1.2). The callee answers
/// after the CANCEL was minted but before its script reads it — a real
/// crossing on the wire. The crossing answer is confirmed on the callee's
/// side (ACK, then BYE), never relayed to a caller whose INVITE already
/// carries its 487.
///
/// Ordering this test depends on (100 ms transit per hop): alice's BYE
/// leaves at t, reaches the stack at t+100 (CANCEL minted, b-leg
/// `Cancelling`) and the CANCEL reaches bob at t+200; bob's 200 leaves at
/// t+50, before that CANCEL, and reaches the stack at t+150, after the BYE.
#[tokio::test(start_paused = true)]
async fn caller_bye_crossing_the_callees_answer_confirms_and_releases_the_callee() {
    let h = Harness::with_transit_delay("b2bua-early-dialog-bye-200-crossing", 100);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5072).start(&h, "b2bua", "127.0.0.1:5082").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── alice BYEs; the stack CANCELs bob and bob's 200 crosses that CANCEL ──
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    advance(50).await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    bye.expect(200).await;
    call.expect(487).await;

    // ── bob: the CANCEL finds an INVITE transaction its 2xx has already
    //    destroyed (§17.2.1), so it draws 481 (§9.2); his 2xx is ACKed and the
    //    dialog it opened is released with a BYE ──
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(481, "Call/Transaction Does Not Exist").await;
    bob.receive("ACK").await;
    let mut b_bye = bob.receive("BYE").await;
    b_bye.respond(200, "OK").await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let seen = start_lines(&alice);
    let invite_2xx = alice
        .wire_view()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .filter(|raw| raw.starts_with("SIP/2.0 200") && cseq_method(raw) == Some("INVITE"))
        .count();
    assert_eq!(invite_2xx, 0, "the crossing 2xx never reaches a caller already rejected: {seen:?}");
    assert_eq!(
        seen.iter().filter(|l| l.starts_with("SIP/2.0 487")).count(),
        1,
        "one 487 on the INVITE: {seen:?}"
    );

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));
    let b_leg = cdrs[0].b_legs.first().expect("one callee leg").leg_id.clone();
    let b_events: Vec<(CdrEventType, Option<String>)> = cdrs[0]
        .events
        .iter()
        .filter(|e| e.leg_id == b_leg)
        .map(|e| (e.event_type, e.reason.clone()))
        .collect();
    let crossing = |kind: CdrEventType| {
        b_events.iter().any(|(k, r)| *k == kind && r.as_deref() == Some("cancel_crossing"))
    };
    assert!(
        crossing(CdrEventType::Answer),
        "the callee's event stream carries the crossing answer: {b_events:?}"
    );
    assert!(crossing(CdrEventType::Bye), "…and the release that answered it: {b_events:?}");

    let _report = h.finish().await;
}

/// The callee BYEs the early dialog it opened — forbidden to it (RFC 3261
/// §15, last paragraph) but survived here — and the caller's own BYE crosses
/// the final that release draws. Every transaction gets exactly one final, no
/// second INVITE final goes out, and the teardown already under way is not
/// restarted.
///
/// The callee's release answers the caller's INVITE in the same turn, so the
/// caller's BYE always lands on a leg that already carries its final: the
/// "terminating, INVITE still unanswered" shape is not reachable this way.
///
/// Ordering this test depends on: bob's BYE is sent first and the simulated
/// network delivers same-instant datagrams in order, so his BYE is processed
/// before alice's and the 503 precedes her BYE. Were alice's BYE processed
/// first, the caller's early-dialog BYE would answer the INVITE 487 instead.
#[tokio::test(start_paused = true)]
async fn callee_early_bye_then_caller_bye_leaves_every_transaction_one_final() {
    let h = Harness::with_transit_delay("b2bua-early-dialog-bye-callee-first", 100);
    // bob's early BYE is the fixture: §15 forbids it to him and the audit
    // charges him for it; the stack's conduct under it is the subject.
    h.waive(
        WaiverScope::rule(
            "no-bye-outside-or-early-dialog",
            "the callee BYEs the early dialog on purpose (RFC 3261 §15 forbids it to the UAS); \
             how the B2BUA survives it is this test's subject",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;
    let alice_addr = alice.addr();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── bob BYEs his early dialog; alice BYEs hers before any final reaches her ──
    let mut b_dialog = b_inv.dialog();
    let mut b_bye = b_dialog.bye().await;
    let mut a_bye = call.send_request(InDialogMethod::Bye).send().await;
    b_bye.expect(200).await;

    // The callee's release answers the INVITE; alice's BYE, arriving after
    // that final, names a dialog the final ended.
    call.expect(503).await; // auto-ACKed (§17.1.1.3)
    a_bye.expect(481).await;

    h.advance(Duration::from_secs(2)).await;
    reaped(&b2bua).await;
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice_addr),
        [503],
        "exactly one INVITE final on alice's wire: the callee's release, answered unanswered"
    );
    let bye_finals = sent(&report.entries(), b2bua.addr, alice_addr, "SIP/2.0 ")
        .into_iter()
        .filter(|(_, raw)| cseq_method(raw) == Some("BYE"))
        .count();
    assert_eq!(bye_finals, 1, "alice's BYE gets exactly one final");
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(
        t.by_leg.as_deref(),
        Some(cdrs[0].b_legs[0].leg_id.as_str()),
        "the callee released first"
    );
}

/// RFC 3262 §3: the caller-facing reliable provisional is retransmitted on
/// this stack's ladder until PRACKed. The caller who BYEs instead of PRACKing
/// ends the setup, and the ladder with it: no copy of the 180 after the 487.
#[tokio::test(start_paused = true)]
async fn caller_bye_on_a_reliable_early_dialog_ends_the_provisional_ladder() {
    let h = Harness::with_transit_delay("b2bua-early-dialog-bye-100rel", 1);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;
    let alice_addr = alice.addr();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv
        .respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", "4711")
        .await;
    let ringing = call.expect(180).await;
    assert!(
        stated_by_response(&ringing, "RSeq").is_some(),
        "the relayed 180 is reliable under this stack's own RSeq"
    );

    // One rung fires, then alice BYEs instead of PRACKing.
    advance(700).await;
    alice.drain().await;
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    bye.expect(200).await;
    call.expect(487).await;

    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    // Well past the 64·T1 bound: a live ladder would have fired many times.
    advance(64 * T1_MS + 2_000).await;
    reaped(&b2bua).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));
    let report = h.finish().await;
    let entries = report.entries();

    let rings = sent(&entries, b2bua.addr, alice_addr, "SIP/2.0 180");
    let released_at = sent(&entries, b2bua.addr, alice_addr, "SIP/2.0 487")
        .first()
        .expect("alice was released 487")
        .0;
    assert!(rings.len() >= 2, "the ladder was live before the BYE: {rings:?}");
    assert!(
        rings.iter().all(|(ms, _)| *ms < released_at),
        "no copy of the 180 after the 487 at {released_at} ms: {:?}",
        rings.iter().map(|(ms, _)| ms).collect::<Vec<_>>()
    );
}

/// A forked callee rings twice under two To-tags (RFC 3261 §12.1.2), so the
/// caller holds two early dialogs. Her BYE on one of them ends the setup:
/// one CANCEL toward the callee (§9.1: CANCEL is per transaction), one 487
/// on the INVITE, and §12.3 ends every early dialog with that non-2xx final.
/// The 487 is matched by branch (§17.1.1) and no rule names its tag, so this
/// stack sends it under the first a-facing tag whichever dialog the BYE named.
#[tokio::test(start_paused = true)]
async fn caller_bye_on_one_of_two_forked_early_dialogs_ends_the_whole_setup() {
    let h = Harness::with_transit_delay("b2bua-early-dialog-bye-forked", 1);
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let p1 = call.expect(180).await;
    let fork1_atag = p1.to().tag().expect("fork 1 a-facing tag").to_string();
    b_inv.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let p2 = call.expect(180).await;
    let fork2_atag = p2.to().tag().expect("fork 2 a-facing tag").to_string();
    assert_ne!(fork1_atag, fork2_atag, "two early dialogs on the caller's face");

    // ── alice BYEs fork 2's early dialog ──
    let mut bye = call.send_request(InDialogMethod::Bye).with_to_tag(&fork2_atag).send().await;
    bye.expect(200).await;
    let rejected = call.expect(487).await;
    assert_eq!(
        rejected.to().tag(),
        Some(fork1_atag.as_str()),
        "the 487 goes out under the first a-facing tag, not the BYE'd dialog's"
    );

    // ── one CANCEL for the one INVITE transaction; the 487 comes from a fork ──
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").with_to_tag("bobfork1").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let a_seen = start_lines(&alice);
    assert_eq!(
        a_seen.iter().filter(|l| l.starts_with("SIP/2.0 487")).count(),
        1,
        "one 487 on the INVITE transaction: {a_seen:?}"
    );
    let b_seen = start_lines(&bob);
    assert_eq!(
        b_seen.iter().filter(|l| l.starts_with("CANCEL ")).count(),
        1,
        "one CANCEL for the one INVITE transaction: {b_seen:?}"
    );
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));

    let _report = h.finish().await;
}

/// The `Reason` the caller states on her early BYE (RFC 3326 §2) rides the
/// CANCEL minted for the callee (§16.6 relay): the callee learns why it
/// stopped ringing.
#[tokio::test(start_paused = true)]
async fn the_callers_early_bye_reason_rides_the_cancel_minted_for_the_callee() {
    let h = Harness::new("b2bua-early-dialog-bye-reason");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut bye =
        call.send_request(InDialogMethod::Bye).with_header("Reason", "Q.850;cause=16").send().await;
    bye.expect(200).await;
    call.expect(487).await;

    let mut b_cxl = bob.receive("CANCEL").await;
    assert_eq!(
        stated(b_cxl.request(), "Reason").as_deref(),
        Some("Q.850;cause=16"),
        "the caller's cause reaches the callee on the minted CANCEL"
    );
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));

    let _report = h.finish().await;
}
