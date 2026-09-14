//! RFC 3261 §13.3.1.4 — the un-ACKed-2xx ladder hands **the response** to the
//! transport layer, not an equivalent one. Two 2xx on one transaction that
//! differ in content are not a retransmission.
//!
//! The B2BUA's a-leg ladder counts correctly (one repeat at T1, only while the
//! caller's ACK is outstanding) but re-composes the datagram from local dialog
//! state, so every header the original carried **by relay from the b-leg** is
//! missing from the repeat: the callee's `P-Charging-Vector` and
//! `P-HKH-RC-HIST` (what the call is billed and audited on) and its declared
//! `Server` / `Allow-Events` / `P-Identifier` / `P-Options`. A caller that lost
//! the first datagram — the only reason the ladder exists — is answered with
//! the stripped copy.
//!
//! Every a-facing INVITE 2xx emit path is put to the same bar, because they
//! reach the ladder through one action and each composes the answer
//! differently: the plain relay, the two `relayFirst18xTo180` masking
//! strategies that still relay the callee's own final (`keep-sdp`,
//! `fake-prack`, the latter substituting cached reliable-18x SDP), and
//! `promote-pem-to-200`, whose 2xx the B2BUA mints itself.
//!
//! Each caller declares a `delayed-automatic` ACK longer than T1, so the ladder
//! fires once and the call still terminates properly.
//!
//! The in-dialog twin (the re-INVITE 2xx's `AckOf2xx` ladder) already re-emits
//! the exact serialized bytes it cached at relay time and is not exercised here.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use scenario_harness::RunReport;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Longer than T1 (500 ms) and well inside the 32 s give-up deadline: exactly
/// one ladder rung fires while the caller holds her ACK.
const HELD_ACK: Duration = Duration::from_millis(700);

/// The headers the callee states on its answer and the B2BUA relays onto the
/// a-leg 2xx. `P-Charging-Vector` and `P-HKH-RC-HIST` are the billing and
/// release-history ones; the rest are the callee's declared capabilities.
const RELAYED: &[(&str, &str)] = &[
    ("P-Charging-Vector", "icid-value=icid-9f2c;orig-ioi=bob.example"),
    ("P-HKH-RC-HIST", "200;src=bob"),
    ("Server", "BobPhone/4.2"),
    ("Allow-Events", "presence, refer"),
    ("P-Identifier", "bob-42"),
    ("P-Options", "sendrecv"),
];

// ── The gate ────────────────────────────────────────────────────────────────

/// The INVITE 2xx datagrams the SUT put on the wire toward the caller, in send
/// order. Filtered to the INVITE transaction so a BYE's 200 never counts.
fn a_leg_invite_2xx(report: &RunReport, sut: SocketAddr, caller: SocketAddr) -> Vec<Vec<u8>> {
    let entries = report.entries();
    entries
        .iter()
        .filter(|e| e.from == sut && e.to == caller)
        .filter(|e| e.raw.starts_with(b"SIP/2.0 200 "))
        .filter(|e| cseq_method(&e.raw).as_deref() == Some("INVITE"))
        .map(|e| e.raw.clone())
        .collect()
}

/// RFC 3261 §13.3.1.4: every later copy is THE response, byte for byte. Reports
/// the header delta on failure — a rebuilt copy silently drops what it could
/// not derive from local state.
fn assert_repeats_are_the_response(lane: &str, copies: &[Vec<u8>]) {
    assert!(
        copies.len() >= 2,
        "{lane}: the held ACK must make the §13.3.1.4 ladder fire, got {} copies of the 2xx",
        copies.len(),
    );
    let original = &copies[0];
    for (n, copy) in copies.iter().enumerate().skip(1) {
        if copy == original {
            continue;
        }
        let before = header_lines(original);
        let after = header_lines(copy);
        let dropped: Vec<&String> = before.iter().filter(|h| !after.contains(h)).collect();
        let gained: Vec<&String> = after.iter().filter(|h| !before.contains(h)).collect();
        panic!(
            "{lane}: RFC 3261 §13.3.1.4 retransmit #{n} is not the 2xx it repeats.\n\
             dropped: {dropped:#?}\ngained: {gained:#?}\n\
             original ({} bytes):\n{}\nrepeat ({} bytes):\n{}",
            original.len(),
            String::from_utf8_lossy(original),
            copy.len(),
            String::from_utf8_lossy(copy),
        );
    }
}

/// Assert the callee's relayed headers reached the caller at all — without them
/// on the original the faithfulness gate above would pass vacuously.
fn assert_relayed_onto(lane: &str, original: &[u8]) {
    let lines = header_lines(original);
    for &(name, value) in RELAYED {
        assert!(
            lines.iter().any(|l| l.eq_ignore_ascii_case(&format!("{name}: {value}"))),
            "{lane}: the callee's {name} must ride the a-leg 2xx, got {lines:#?}",
        );
    }
}

/// The header lines of a SIP datagram, start line and body excluded.
fn header_lines(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The method a datagram's `CSeq` names.
fn cseq_method(raw: &[u8]) -> Option<String> {
    header_lines(raw)
        .into_iter()
        .find(|l| l.to_ascii_lowercase().starts_with("cseq:"))
        .and_then(|l| l.split_whitespace().next_back().map(str::to_string))
}

// ── The lanes ───────────────────────────────────────────────────────────────

/// The plain relay: the callee's own 200 carries the six headers onto the a-leg.
#[tokio::test(start_paused = true)]
async fn a_relayed_answer_is_retransmitted_whole() {
    let h = Harness::new("unacked-2xx-faithful-relay");
    let alice = h.agent("alice", "127.0.0.1:5301").await;
    let bob = h.agent("bob", "127.0.0.1:5311").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5311).start(&h, "b2bua", "127.0.0.1:5321").await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).delayed_ack(HELD_ACK).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    answer_stating(&mut uas, Some(ANSWER)).await;
    call.expect(200).await;
    // Alice holds her ACK past T1 — the ladder fires — then confirms, and that
    // ACK is what the callee gets (RFC 3261 §13.2.2.4: one ACK per 2xx).
    let mut dialog = call.ack_delayed().await;
    bob.receive("ACK").await;
    alice.drain().await;

    teardown(&h, &b2bua, &bob, &mut dialog).await;
    let report = h.finish().await;

    let copies = a_leg_invite_2xx(&report, b2bua.addr, alice.addr());
    assert_relayed_onto("relay", &copies[0]);
    assert_repeats_are_the_response("relay", &copies);
}

/// `keep-sdp` masking: the 18x machine downgrades the provisional but the
/// callee's own final still relays, and it stays armed over the 2xx.
#[tokio::test(start_paused = true)]
async fn a_masked_call_answer_is_retransmitted_whole() {
    let h = Harness::new("unacked-2xx-faithful-keep-sdp");
    let alice = h.agent("alice", "127.0.0.1:5302").await;
    let bob = h.agent("bob", "127.0.0.1:5312").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5312, RelayFirst18xStrategy::KeepSdp)
        .start(&h, "b2bua", "127.0.0.1:5322")
        .await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).delayed_ack(HELD_ACK).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").with_sdp(ANSWER).await;
    call.expect(180).await;
    answer_stating(&mut uas, Some(ANSWER)).await;
    call.expect(200).await;
    let mut dialog = call.ack_delayed().await;
    bob.receive("ACK").await;
    alice.drain().await;

    teardown(&h, &b2bua, &bob, &mut dialog).await;
    let report = h.finish().await;

    let copies = a_leg_invite_2xx(&report, b2bua.addr, alice.addr());
    assert_relayed_onto("keep-sdp", &copies[0]);
    assert_repeats_are_the_response("keep-sdp", &copies);
}

/// `fake-prack`: the B2BUA PRACKs the callee itself and substitutes the cached
/// reliable-18x SDP into the answer, so the a-leg 2xx is neither the callee's
/// datagram nor derivable from the a-leg INVITE alone.
#[tokio::test(start_paused = true)]
async fn a_fake_pracked_answer_is_retransmitted_whole() {
    let h = Harness::new("unacked-2xx-faithful-fake-prack");
    let alice = h.agent("alice", "127.0.0.1:5303").await;
    let bob = h.agent("bob", "127.0.0.1:5313").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 5313, RelayFirst18xStrategy::FakePrack)
            .start(&h, "b2bua", "127.0.0.1:5323")
            .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, timer")
        .delayed_ack(HELD_ACK)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    // Bob's 200 has no body: alice's carries the cached 18x SDP instead.
    answer_stating(&mut uas, None).await;
    let ok = call.expect(200).await;
    assert!(!ok.body().is_empty(), "the a-leg 200 carries the cached reliable-18x SDP");
    let mut dialog = call.ack_delayed().await;
    bob.receive("ACK").await;
    alice.drain().await;

    teardown(&h, &b2bua, &bob, &mut dialog).await;
    let report = h.finish().await;

    let copies = a_leg_invite_2xx(&report, b2bua.addr, alice.addr());
    assert_relayed_onto("fake-prack", &copies[0]);
    assert_repeats_are_the_response("fake-prack", &copies);
}

/// `promote-pem-to-200`: the 2xx is minted by the B2BUA from the callee's
/// early-media 183, so it reaches the ladder through the answer-a-new-dialog
/// path rather than the relay one.
#[tokio::test(start_paused = true)]
async fn a_promoted_early_media_answer_is_retransmitted_whole() {
    let h = Harness::new("unacked-2xx-faithful-promote-pem");
    let alice = h.agent("alice", "127.0.0.1:5304").await;
    let bob = h.agent("bob", "127.0.0.1:5314").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 5314, RelayFirst18xStrategy::PromotePemTo200)
            .start(&h, "b2bua", "127.0.0.1:5324")
            .await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).delayed_ack(HELD_ACK).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // The 183 the promotion fires on states the same relayed headers a final
    // would: the synthetic 200 is the caller's answer, and its repeat is bound
    // to whatever that answer carried.
    let mut early = uas.respond(183, "Session Progress").with_header("P-Early-Media", "sendrecv");
    for &(name, value) in RELAYED {
        early = early.with_header(name, value);
    }
    early.with_sdp(ANSWER).await;
    call.expect(200).await;

    let mut dialog = call.ack_delayed().await;
    alice.drain().await;

    // Bob's real 200 carries the same SDP — silent confirm, no resync.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive("ACK").await;

    teardown(&h, &b2bua, &bob, &mut dialog).await;
    let report = h.finish().await;

    let copies = a_leg_invite_2xx(&report, b2bua.addr, alice.addr());
    assert_repeats_are_the_response("promote-pem", &copies);
}

// ── Shared scenario steps ───────────────────────────────────────────────────

/// The callee answers stating the six relayed headers (and an optional answer
/// body — `fake-prack` answers without one).
async fn answer_stating(uas: &mut scenario_harness::ServerTxn, sdp: Option<&str>) {
    let mut respond = uas.respond(200, "OK");
    for &(name, value) in RELAYED {
        respond = respond.with_header(name, value);
    }
    if let Some(sdp) = sdp {
        respond = respond.with_sdp(sdp);
    }
    respond.await;
}

/// Caller-initiated BYE, both legs reaped.
async fn teardown(
    h: &Harness,
    b2bua: &B2buaSut,
    bob: &scenario_harness::Agent,
    dialog: &mut scenario_harness::Dialog,
) {
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let _ = h;
}
