//! An unacknowledged reliable provisional to a **re-INVITE** is rejected by
//! the party that answered the re-INVITE, and the reject ends the
//! renegotiation, not the call.
//!
//! RFC 3262 §3 bounds the ladder at 64·T1, then "the UAS SHOULD reject the
//! original request with a 5xx". The provisional this stack shows is a relay of
//! the answering party's own, whose PRACK is owed end to end, so that party is
//! the UAS whose reject answers the silence: this stack's ladder stops at its
//! bound, and the 5xx it relays back is the re-INVITE's final. RFC 3261 §14.1
//! leaves a failed re-INVITE's dialog in the state it held before, so the
//! established call carries on.
//!
//! Both directions are pinned: the caller's renegotiation and the callee's.

use std::net::SocketAddr;

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::run::RunReport;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net_testkit::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// Finish the run and render its SIP call-flow artifacts (`<name>.html` +
/// `.svg` + `.global.txt`) under `target/seq-reports/prack-remainder/<scenario>/`,
/// so the ladder each assertion below reads has a sequence diagram beside it.
fn write_flow_report(report: &RunReport) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports/prack-remainder")
        .join(&report.scenario_name);
    let paths = scenario_harness::report::write_all(report, &dir).expect("write report");
    if let Some(html) = paths.iter().find(|p| p.extension().is_some_and(|e| e == "html")) {
        eprintln!("prack-remainder report: {}", html.display());
    }
}

/// Bob's own sequence — far from anything this stack mints first.
const BOB_RSEQ: u32 = 4711;

/// RFC 3261 T1, and §3's 64·T1 bound.
const T1_MS: u64 = 500;
const BOUND_MS: u64 = 64 * T1_MS;

/// The margin an advance takes past a deadline it must cross.
const MARGIN_MS: u64 = 50;

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
/// PRACKs. Past 64·T1 bob rejects his re-INVITE transaction and the reject
/// reaches alice; the established call stands, and alice's own BYE is what
/// ends it.
///
/// ```text
///   INVITE(100rel) → 180 → 200 → ACK
///   re-INVITE(100rel) → 183(100rel,RSeq 4711) ← 183(100rel,RSeq n)
///                       [alice never PRACKs — the ladder runs to 64·T1]
///   bob's bound: ← 500(re-INVITE) ← 500 ; ACK               [the renegotiation]
///                BYE → 200(BYE)                             [the call, alice's]
/// ```
#[tokio::test(start_paused = true)]
async fn the_answering_party_rejects_the_callers_reinvite() {
    let h = Harness::with_transit_delay("b2bua-prack-giveup-in-dialog", 0);
    // Alice deliberately never PRACKs — that withholding IS the fixture, and it
    // is the only way to reach the bound. Every other bind stays gated.
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice deliberately never PRACKs her re-INVITE's reliable provisional, so the a-face \
             ladder runs to its 64·T1 bound (RFC 3262 §3) — the caller's silence is this test's \
             subject",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5181").await;
    let bob = h.agent("bob", "127.0.0.1:5182").await;
    // The keepalive is not this test's subject, and the harness baseline probes
    // at 30 s — inside §3's 32 s bound. Push it past the window so the ladder
    // is the only clock in play.
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

    // ── alice withholds her PRACK past the bound: nothing from this stack ──
    h.advance(Duration::from_millis(BOUND_MS + 2 * MARGIN_MS)).await;
    alice.drain().await;

    // bob's §3 reject answers his re-INVITE transaction, relayed to alice; the
    // PRACK this stack owes bob leaves at that final on the confirmed dialog
    // (RFC 3262 §4).
    re_uas.respond(500, "Server Internal Error").await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    reinv.expect(500).await;

    // ── the call is untouched: it ends on alice's own BYE, not the timer's ──
    h.advance(Duration::from_millis(2 * MARGIN_MS)).await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    let report = h.finish().await;
    write_flow_report(&report);
    let entries = report.entries();
    assert!(
        requests_to(&entries, b2bua.addr, alice_addr, "BYE").is_empty(),
        "RFC 3261 §14.1: a failed re-INVITE leaves the dialog as it was — the reject ends \
         the transaction, it does not BYE the caller",
    );
    assert!(
        requests_to(&entries, b2bua.addr, bob_addr, "CANCEL").is_empty(),
        "bob ends his own re-INVITE transaction: nothing CANCELs it",
    );
    // Toward the callee, alice's own BYE relayed and nothing else.
    let alice_bye = requests_to(&entries, alice_addr, b2bua.addr, "BYE");
    let relayed = requests_to(&entries, b2bua.addr, bob_addr, "BYE");
    assert_eq!(alice_bye.len(), 1, "alice ends the call once: {alice_bye:?}");
    assert_eq!(relayed.len(), 1, "and the callee sees hers, relayed, and no other: {relayed:?}",);
    b2bua_harness::settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The mirror of the cell above, with the callee originating: bob re-INVITEs
/// offering `100rel`, alice answers reliably, and BOB never PRACKs. Alice's §3
/// reject answers HIS re-INVITE, relayed to him; the established call is again
/// untouched.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   re-INVITE(100rel) bob→ 183(100rel,RSeq n) →bob ; 183(100rel,RSeq 4711) ←alice
///                       [bob never PRACKs — the ladder runs to 64·T1]
///   alice's bound: 500(re-INVITE) →bob ; 500 ←alice ; ACK     [the renegotiation]
///                  BYE bob→ → 200(BYE)                        [the call, bob's]
/// ```
#[tokio::test(start_paused = true)]
async fn the_answering_party_rejects_the_callees_reinvite() {
    let h = Harness::with_transit_delay("b2bua-prack-giveup-callee-reinvite", 0);
    // Bob deliberately never PRACKs — that withholding IS the fixture, and it is
    // the only way to reach the bound. Every other bind stays gated.
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "bob deliberately never PRACKs the reliable provisional answering his own \
             re-INVITE, so the ladder shown toward him runs to its 64·T1 bound \
             (RFC 3262 §3) — his silence is this test's subject",
        )
        .on_party("bob"),
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

    // ── bob withholds his PRACK past the bound: nothing from this stack ──
    h.advance(Duration::from_millis(BOUND_MS + 2 * MARGIN_MS)).await;
    bob.drain().await;

    // alice's §3 reject answers bob's re-INVITE transaction, relayed to him;
    // the PRACK this stack owes alice leaves at that final (RFC 3262 §4).
    re_uas.respond(500, "Server Internal Error").await;
    alice.receive("PRACK").await.respond(200, "OK").await;
    reinv.expect(500).await;

    // ── the call is untouched: it ends on bob's own BYE ──
    h.advance(Duration::from_millis(2 * MARGIN_MS)).await;
    let mut bye = bob_dialog.bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    let report = h.finish().await;
    write_flow_report(&report);
    let entries = report.entries();
    assert!(
        requests_to(&entries, b2bua.addr, bob_addr, "BYE").is_empty(),
        "RFC 3261 §14.1: the reject ends the callee's transaction, it does not BYE him",
    );
    assert!(
        requests_to(&entries, b2bua.addr, alice_addr, "CANCEL").is_empty(),
        "alice ends her own answer to the re-INVITE: nothing CANCELs it",
    );
    let bob_bye = requests_to(&entries, bob_addr, b2bua.addr, "BYE");
    let relayed = requests_to(&entries, b2bua.addr, alice_addr, "BYE");
    assert_eq!(bob_bye.len(), 1, "bob ends the call once: {bob_bye:?}");
    assert_eq!(relayed.len(), 1, "and the caller sees his, relayed, and no other: {relayed:?}",);
    b2bua_harness::settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
