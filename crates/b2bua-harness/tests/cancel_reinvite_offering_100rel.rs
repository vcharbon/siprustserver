//! A CANCELled re-INVITE that drew a reliable provisional stops its ladder
//! (issue 264 item G — the untested half of 109's per-transaction cancel).
//!
//! A CANCELled re-INVITE resolves through `ResolveCancelledReinvite` and never
//! reaches the relayed-final hook, so the ladder this stack armed toward the
//! caller for that transaction's reliable provisional is cancelled on its own
//! path. RFC 3262 §3 has the retransmissions run until the provisional is
//! PRACKed or the transaction ends; the 487 ends it, so no copy may follow.
//! RFC 3261 §14.1 leaves the dialog in its prior state — the call carries on.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};
use scenario_harness::run::RunReport;
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;
use sip_net::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

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

/// RFC 3261 T1: §3's first ladder rung falls here, and the second one T1 later.
const T1_MS: u64 = 500;
const STEP_MS: u64 = 50;

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

/// Every copy of `status` the SUT put on `to`'s wire.
fn responses_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr, status: u16) -> usize {
    let head = format!("SIP/2.0 {status} ");
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .count()
}

/// ```text
///   INVITE(100rel) → 180 → 200 → ACK
///   re-INVITE(100rel) → 180(100rel,RSeq 4711) ← 180(100rel,RSeq n)
///   CANCEL → 200(CANCEL) ; CANCEL → 487 ← 487 → ACK
///   [no further copy of the 180 — the transaction's ladder died with it]
///   BYE → 200(BYE)
/// ```
#[tokio::test(start_paused = true)]
async fn cancelling_a_reinvite_stops_its_reliable_provisional_ladder() {
    let h = Harness::with_transit_delay("b2bua-cancel-reinvite-100rel", 0);
    // Alice CANCELs instead of PRACKing — the withholding IS the fixture.
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice CANCELs the renegotiation instead of PRACKing its reliable provisional \
             (RFC 3261 §9.1) — the unacknowledged provisional is this test's subject",
        )
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:5184").await;
    let bob = h.agent("bob", "127.0.0.1:5185").await;
    // The keepalive is not this test's subject; push its probe past the window.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5185)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5186")
        .await;
    let alice_addr = alice.addr();

    // ── an ordinary call ──
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

    // ── a CANCELlable re-INVITE that offers 100rel ──
    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .with_header("Supported", "100rel")
        .send_cancellable()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    re_uas.respond(180, "Ringing").reliable(BOB_RSEQ).await;
    let ring = reinv.expect(180).await;
    assert!(
        ring.header::<RSeq>().is_some(),
        "the renegotiation's provisional reaches the caller reliably — she offered 100rel",
    );

    // ── alice CANCELs it before the first ladder rung falls ──
    let mut cancel = reinv.cancel().await;
    cancel.expect(200).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    re_uas.respond(487, "Request Terminated").await;
    reinv.expect(487).await;

    // ── two ladder rungs' worth of silence: the transaction is over ──
    advance(3 * T1_MS).await;

    // ── the call is untouched; alice ends it herself ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    let report = h.finish().await;
    write_flow_report(&report);
    assert_eq!(
        responses_to(&report.entries(), b2bua.addr, alice_addr, 180),
        2,
        "one copy for the call's own ring and one for the renegotiation's — the CANCELled \
         transaction's ladder retransmits nothing after its 487 (RFC 3262 §3)",
    );
    b2bua.assert_fully_reaped();
}
