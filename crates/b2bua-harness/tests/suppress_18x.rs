//! suppress-18x — `relayFirst18xTo180` strategy `drop-sdp` (wire `true`). Port of
//! `tests/scenarios/suppress-18x.ts`.
//!
//! The B2BUA rewrites the first 18x from any b-leg into a bare 180 (no SDP / no
//! 100rel), suppresses later 18x, and reuses the first 180's To-tag on the 200
//! OK. A reliable 1xx is PRACKed by the B2BUA itself (alice never sees it).
//!
//! Failover cases (`failoverNoAnswer`, `failoverReject`) are NOT ported: the
//! Rust B2BUA has no SIP b-leg failover (`/call/failure`) yet — see
//! docs/plan/slice3-design.md and MIGRATION_STATUS.md.

use b2bua_harness::B2buaSut;
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn has_token(value: Option<&str>, token: &str) -> bool {
    value
        .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        .unwrap_or(false)
}

#[tokio::test]
async fn basic() {
    let h = Harness::with_transit_delay("suppress-18x-basic", 0);
    let alice = h.agent("alice", "127.0.0.1:5601").await;
    let bob = h.agent("bob", "127.0.0.1:5611").await;
    let b2bua = B2buaSut::route_all_to_with_18x(
        &h,
        "b2bua",
        "127.0.0.1:5621",
        "127.0.0.1",
        5611,
        RelayFirst18xStrategy::DropSdp,
    )
    .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, timer")
        .through(b2bua.addr)
        .send()
        .await;

    // Bob receives the INVITE — 100rel must be stripped from Supported (drop-sdp).
    let mut uas = bob.receive("INVITE").await;
    assert!(
        !has_token(get_header(&uas.request().headers, "supported"), "100rel"),
        "100rel stripped from bob's Supported",
    );

    // Bob sends a reliable 183 with SDP.
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;

    // Alice sees a bare 180 — no body, no Require:100rel, no RSeq.
    let p180 = call.expect(180).await;
    assert!(p180.body.is_empty(), "bare 180 has no body");
    assert!(
        !has_token(get_header(&p180.headers, "require"), "100rel"),
        "no Require:100rel on bare 180",
    );
    assert!(get_header(&p180.headers, "rseq").is_none(), "no RSeq on bare 180");
    let first_to_tag = p180.to.tag.clone().expect("180 has a To-tag");

    // The B2BUA PRACKs bob (alice never saw the reliable provisional).
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(
        get_header(&prack.request().headers, "rack").map(|r| r.split_whitespace().collect::<Vec<_>>()),
        Some(vec!["1", "1", "INVITE"]),
        "RAck = rseq 1, INVITE cseq 1",
    );
    prack.respond(200, "OK").await;

    // Bob sends another 180 — suppressed (alice receives nothing more here).
    uas.respond(180, "Ringing").await;

    // Bob answers with SDP; alice's 200 reuses the first 180's To-tag.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to.tag.as_deref(), Some(first_to_tag.as_str()), "200 To-tag == 180 To-tag");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// No policy → normal behaviour: every 180 is relayed verbatim (no suppression,
/// no bare-180 downgrade). Regression guard the new code stays off the default
/// path. (TS `suppress18xDisabled`.)
#[tokio::test]
async fn disabled() {
    let h = Harness::with_transit_delay("suppress-18x-disabled", 0);
    let alice = h.agent("alice", "127.0.0.1:5602").await;
    let bob = h.agent("bob", "127.0.0.1:5612").await;
    // route_all_to → no relay_first_18x feature.
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5622", "127.0.0.1", 5612).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;

    let mut uas = bob.receive("INVITE").await;

    // Two plain 180s — both relayed normally.
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}
