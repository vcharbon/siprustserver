//! A responder's RETRANSMISSION of a reliable provisional this stack
//! acknowledged itself draws nothing new — no second copy toward the
//! originator, no second PRACK toward the responder (issue 264 items B and C).
//!
//! RFC 3262 §3 makes the responder repeat its reliable provisional until it is
//! PRACKed, so a copy crossing the acknowledgement in flight is ORDINARY, not a
//! defect. §4 makes every copy after the first a retransmission the receiver
//! discards. This stack acknowledges on the responder's behalf wherever the
//! originator never offered `100rel` (`relay_response`'s strip branch) or the
//! 18x-masking policy hides the provisional outright (`relay_first_18x`), so on
//! both paths the repeat must die where it arrives: one PRACK per reliable
//! provisional, whatever the responder's clock does.
//!
//! A second PRACK is not merely noise. It names the same `(RSeq, CSeq)` on a
//! fresh CSeq, and §3 has the responder answer 481 to a PRACK matching no
//! UNACKNOWLEDGED provisional — the 481 whose handling
//! `prack_481_keeps_the_call.rs` pins.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::B2buaSut;
use call::features::RelayFirst18xStrategy;
use sip_net::RecordedSipEntry;
use scenario_harness::Harness;
use scenario_harness::run::RunReport;
use sip_message::generators::InDialogMethod;
use sip_message::header::Supported;

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

/// Every request of `method` the SUT put on `to`'s wire.
fn requests_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr, method: &str) -> usize {
    let head = format!("{method} ");
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .count()
}

/// Every copy of `status` the SUT put on `to`'s wire.
fn responses_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr, status: u16) -> usize {
    let head = format!("SIP/2.0 {status} ");
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .count()
}

/// A decision that DECLARES `Supported: 100rel` toward the originated face, so
/// the callee answers reliably although the caller offered nothing.
fn decision_offering_100rel_toward_bob(port: u16) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", port);
                r.features.advertise_capabilities = Some(call::features::AdvertiseCapabilitiesFeature {
                    toward_originator: None,
                    toward_originated: Some(call::features::AdvertisedCapabilities {
                        allow: None,
                        supported: Some(vec!["100rel".to_string()]),
                    }),
                });
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// **Item B — the relay path.** Alice re-INVITEs offering no `100rel`; the
/// declaration offers it to bob, who answers reliably. This stack strips the
/// reliability toward alice and PRACKs bob itself. Bob's §3 ladder then repeats
/// the SAME provisional: alice must see nothing new and bob must draw no second
/// PRACK.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   re-INVITE(no 100rel) → [b2bua: Supported:100rel] → 183(100rel,RSeq 4711)
///          ← 183(plain) ; b2bua PRACK → 200(PRACK)
///                        → 183(100rel,RSeq 4711)   [bob's §3 repeat]
///          ← nothing     ; NO second PRACK
///          → 200(INVITE) → ACK → BYE → 200(BYE)
/// ```
#[tokio::test]
async fn a_repeat_of_a_stack_pracked_provisional_draws_no_second_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-repeat-relay", 0);
    let alice = h.agent("alice", "127.0.0.1:5171").await;
    let bob = h.agent("bob", "127.0.0.1:5172").await;
    let b2bua = B2buaSut::builder(decision_offering_100rel_toward_bob(5172))
        .start(&h, "b2bua", "127.0.0.1:5173")
        .await;
    let (alice_addr, bob_addr) = (alice.addr(), bob.addr());

    // ── an ordinary call, established without reliability in play ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs, offering NO reliable provisionals ──
    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .with_header("Supported", "timer")
        .send()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    assert!(
        re_uas
            .request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "the declared Supported: 100rel reaches the callee on the relayed re-INVITE",
    );

    // Bob answers reliably; alice is shown the ordinary copy; this stack PRACKs.
    re_uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(REANSWER).await;
    reinv.expect(183).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    // ── bob's §3 ladder repeats the SAME provisional (his copy crossed the
    // PRACK in flight) — a retransmission, not a new rung ──
    re_uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(REANSWER).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "the repeat is a retransmission of a provisional this stack has already acknowledged \
         (RFC 3262 §4) — it draws no second PRACK",
    );

    // ── bob answers the re-INVITE; alice ACKs the 2xx ──
    re_uas.respond(200, "OK").with_sdp(REANSWER).await;
    let final_200 = reinv.expect(200).await;
    let reinvite_cseq = final_200.cseq().seq();
    alice_dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    write_flow_report(&report);
    let entries = report.entries();
    assert_eq!(
        requests_to(&entries, b2bua.addr, bob_addr, "PRACK"),
        1,
        "one reliable provisional is acknowledged once (RFC 3262 §4): a repeat of a provisional \
         this stack already PRACKed draws no second PRACK",
    );
    assert_eq!(
        responses_to(&entries, b2bua.addr, alice_addr, 183),
        1,
        "the originator sees the stripped provisional once — bob's repeat is absorbed here, \
         it is not hers to receive again",
    );
    b2bua.assert_fully_reaped();
}

/// **Item C — the 18x-masking path.** `relayFirst18xTo180` hides bob's reliable
/// provisional behind a bare 180 and PRACKs him. His §3 repeat must draw no
/// second PRACK either: `relay_first_18x` acknowledges each reliable 1xx it
/// sees, retransmissions included.
///
/// ```text
///   INVITE(100rel) → [b2bua strips 100rel] → 183(100rel,RSeq 4711)
///          ← bare 180 ; b2bua PRACK → 200(PRACK)
///                        → 183(100rel,RSeq 4711)   [bob's §3 repeat]
///          ← nothing   ; NO second PRACK
///          → 200 → ACK → BYE → 200(BYE)
/// ```
#[tokio::test]
async fn a_repeat_behind_the_18x_mask_draws_no_second_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-repeat-masked", 0);
    let alice = h.agent("alice", "127.0.0.1:5174").await;
    let bob = h.agent("bob", "127.0.0.1:5175").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5175, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5176")
        .await;
    let bob_addr = bob.addr();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // Bob answers reliably; alice sees the bare 180; this stack PRACKs.
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(ANSWER).await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    // Bob's §3 ladder repeats the SAME provisional.
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(ANSWER).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "the masking path has already acknowledged this provisional (RFC 3262 §4) — its repeat \
         draws no second PRACK",
    );

    // ── bob answers; alice ACKs; teardown ──
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    write_flow_report(&report);
    assert_eq!(
        requests_to(&report.entries(), b2bua.addr, bob_addr, "PRACK"),
        1,
        "the masking path acknowledges one reliable provisional once (RFC 3262 §4) — a repeat \
         is a retransmission, not a rung owed its own PRACK",
    );
    b2bua.assert_fully_reaped();
}
