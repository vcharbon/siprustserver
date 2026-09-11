//! An unacknowledged reliable provisional to a **re-INVITE** ends the
//! renegotiation, not the call (issue 264 item D).
//!
//! RFC 3262 §3 bounds the ladder at 64·T1 and then answers the silence:
//! "the UAS SHOULD reject the original request with a 5xx". On an initial
//! INVITE the original request IS the call, and the reject is a teardown
//! (`prack_reliable_ladder.rs::the_ladder_gives_up_at_64_t1`). On a re-INVITE
//! it is one in-dialog transaction: RFC 3261 §14.1 leaves a failed re-INVITE's
//! dialog in the state it held before, so the 5xx answers that transaction and
//! the established call carries on. The relayed re-INVITE still pending toward
//! the callee ends with it, transaction-scoped (§9.1) — the renegotiation is
//! over on both faces, the dialogs are not.
//!
//! Both directions are pinned: the caller's renegotiation and the callee's. The
//! rule resolves the provisional to whichever leg it was SHOWN on (issue 109),
//! so the face that owes the PRACK is the face the 5xx answers, and the CANCEL
//! goes to the other one.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::run::RunReport;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// Finish the run and render its SIP call-flow artifacts (`<name>.html` +
/// `.svg` + `.global.txt`) under `target/seq-reports/prack-remainder/`, so the
/// ladder each assertion below reads has a sequence diagram beside it.
fn write_flow_report(report: &RunReport) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports/prack-remainder");
    let paths = scenario_harness::report::write_all(report, &dir).expect("write report");
    if let Some(html) = paths.iter().find(|p| p.extension().is_some_and(|e| e == "html")) {
        eprintln!("prack-remainder report: {}", html.display());
    }
}

/// Bob's own sequence — far from anything this stack mints first.
const BOB_RSEQ: u32 = 4711;

/// RFC 3261 T1, and §3's 64·T1 give-up bound.
const T1_MS: u64 = 500;
const GIVE_UP_MS: u64 = 64 * T1_MS;

/// The advance step: small enough that no deadline in play is crossed blind.
const STEP_MS: u64 = 50;

/// Advance virtual time by `ms`, letting the simulated pipeline run at each
/// instant it crosses (see `prack_reliable_ladder.rs` for why the step is small).
async fn advance(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(STEP_MS);
        sip_clock::testkit::settle().await;
        tokio::time::advance(Duration::from_millis(step)).await;
        sip_clock::testkit::settle().await;
        left -= step;
    }
}

/// When each request of `method` went from `from` to `to`, in send order.
fn requests_to(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
    method: &str,
) -> Vec<u64> {
    let head = format!("{method} ");
    let mut out: Vec<u64> = entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .map(|e| e.sent_ms)
        .collect();
    out.sort_unstable();
    out
}

/// Alice re-INVITEs offering `100rel`, bob answers reliably, and alice never
/// PRACKs. At 64·T1 the RE-INVITE is rejected and the pending relay toward bob
/// is CANCELled; the established call stands, and alice's own BYE is what ends
/// it.
///
/// ```text
///   INVITE(100rel) → 180 → 200 → ACK
///   re-INVITE(100rel) → 183(100rel,RSeq 4711) ← 183(100rel,RSeq n)
///                       [alice never PRACKs — the ladder runs to 64·T1]
///   64·T1: ← 504(re-INVITE) ; CANCEL → 487                [the renegotiation]
///          BYE → 200(BYE)                                 [the call, alice's]
/// ```
#[tokio::test(start_paused = true)]
async fn an_unacked_reliable_provisional_to_a_reinvite_ends_the_renegotiation_only() {
    let h = Harness::with_transit_delay("b2bua-prack-giveup-in-dialog", 0);
    // Alice deliberately never PRACKs — that withholding IS the fixture, and it
    // is the only way to reach the give-up bound. Every other bind stays gated.
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice deliberately never PRACKs her re-INVITE's reliable provisional, so the a-face \
             ladder runs to its 64·T1 bound (RFC 3262 §3) — the caller's silence is this test's \
             subject",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5181").await;
    let bob = h.agent("bob", "127.0.0.1:5182").await;
    // The keepalive is not this test's subject, and the harness baseline probes
    // at 30 s — inside §3's 32 s bound. Push it past the window so the ladder
    // and its give-up are the only clocks in play.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5182)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5183")
        .await;
    let (alice_addr, bob_addr) = (alice.addr(), bob.addr());

    // ── an ordinary call, established on an unreliable ring ──
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs, offering 100rel; bob answers reliably ──
    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .with_header("Supported", "100rel")
        .send()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    re_uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(REANSWER).await;
    reinv.expect(183).await;

    // ── alice withholds her PRACK to the bound; drain the rungs so what she
    // reads next is the give-up's own answer ──
    advance(GIVE_UP_MS - T1_MS + 2 * STEP_MS).await;
    alice.drain().await;

    // §3's reject answers the RE-INVITE, and the pending relay toward bob ends
    // with it — transaction-scoped (RFC 3261 §9.1), leg state untouched.
    advance(T1_MS + 2 * STEP_MS).await;
    reinv.expect(504).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    re_uas.respond(487, "Request Terminated").await;

    // ── the call is untouched: it ends on alice's own BYE, not the timer's ──
    advance(2 * STEP_MS).await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    let report = h.finish().await;
    write_flow_report(&report);
    let entries = report.entries();
    assert!(
        requests_to(&entries, b2bua.addr, alice_addr, "BYE").is_empty(),
        "RFC 3261 §14.1: a failed re-INVITE leaves the dialog as it was — the give-up rejects \
         the transaction, it does not BYE the caller",
    );
    // Toward the callee, alice's own BYE relayed and nothing else: a give-up
    // teardown would have put a second one on his wire at 64·T1.
    let alice_bye = requests_to(&entries, alice_addr, b2bua.addr, "BYE");
    let relayed = requests_to(&entries, b2bua.addr, bob_addr, "BYE");
    assert_eq!(alice_bye.len(), 1, "alice ends the call once: {alice_bye:?}");
    assert_eq!(relayed.len(), 1, "and the callee sees hers, relayed, and no other: {relayed:?}",);
    b2bua.assert_fully_reaped();
}

/// The mirror of the cell above, with the callee originating: bob re-INVITEs
/// offering `100rel`, alice answers reliably, and BOB never PRACKs. §3's reject
/// answers HIS re-INVITE and the relay toward alice is CANCELled; the
/// established call is again untouched.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   re-INVITE(100rel) bob→ 183(100rel,RSeq n) →bob ; 183(100rel,RSeq 4711) ←alice
///                       [bob never PRACKs — the ladder runs to 64·T1]
///   64·T1: 504(re-INVITE) →bob ; CANCEL →alice → 487        [the renegotiation]
///          BYE bob→ → 200(BYE)                              [the call, bob's]
/// ```
#[tokio::test(start_paused = true)]
async fn the_give_up_answers_the_face_that_owed_the_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-giveup-callee-reinvite", 0);
    // Bob deliberately never PRACKs — that withholding IS the fixture, and it is
    // the only way to reach the give-up bound. Every other bind stays gated.
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "bob deliberately never PRACKs the reliable provisional answering his own \
             re-INVITE, so the ladder shown toward him runs to its 64·T1 bound \
             (RFC 3262 §3). His silence charges the B2BUA bind too: the acknowledgement \
             toward alice is bob's to give and never came, so the stack has none to \
             relay — §3's reject is the answer it owes instead, and it sends it",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5187").await;
    let bob = h.agent("bob", "127.0.0.1:5188").await;
    // The keepalive is not this test's subject; push its probe past the window.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5188)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5189")
        .await;
    let (alice_addr, bob_addr) = (alice.addr(), bob.addr());

    // ── an ordinary call, established on an unreliable ring ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── the CALLEE re-INVITEs, offering 100rel; alice answers reliably ──
    let mut bob_dialog = uas.dialog();
    let mut reinv = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .with_header("Supported", "100rel")
        .send()
        .await;
    let mut re_uas = alice.receive("INVITE").await;
    re_uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(REANSWER).await;
    reinv.expect(183).await;

    // ── bob withholds his PRACK to the bound; drain the rungs shown toward him ──
    advance(GIVE_UP_MS - T1_MS + 2 * STEP_MS).await;
    bob.drain().await;

    // §3's reject answers the face that owed the PRACK — bob's — and the relay
    // toward alice ends with it.
    advance(T1_MS + 2 * STEP_MS).await;
    reinv.expect(504).await;
    alice.receive("CANCEL").await.respond(200, "OK").await;
    re_uas.respond(487, "Request Terminated").await;

    // ── the call is untouched: it ends on bob's own BYE ──
    advance(2 * STEP_MS).await;
    let mut bye = bob_dialog.bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    let report = h.finish().await;
    write_flow_report(&report);
    let entries = report.entries();
    assert!(
        requests_to(&entries, b2bua.addr, bob_addr, "BYE").is_empty(),
        "RFC 3261 §14.1: the give-up rejects the callee's transaction, it does not BYE him",
    );
    let bob_bye = requests_to(&entries, bob_addr, b2bua.addr, "BYE");
    let relayed = requests_to(&entries, b2bua.addr, alice_addr, "BYE");
    assert_eq!(bob_bye.len(), 1, "bob ends the call once: {bob_bye:?}");
    assert_eq!(relayed.len(), 1, "and the caller sees his, relayed, and no other: {relayed:?}",);
    b2bua.assert_fully_reaped();
}
