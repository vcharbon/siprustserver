//! suppress-18x — `relayFirst18xTo180` strategy `drop-sdp` (wire `true`). Port of
//! `tests/scenarios/suppress-18x.ts`.
//!
//! The B2BUA rewrites the first 18x from any b-leg into a bare 180 (no SDP / no
//! 100rel), suppresses later 18x, and reuses the first 180's To-tag on the 200
//! OK. A reliable 1xx is PRACKed by the B2BUA itself (alice never sees it).
//!
//! Failover cases (`failoverNoAnswer`, `failoverReject`) ride the `/call/failure`
//! b-leg failover path: the first 180's To-tag must survive the leg swap (the
//! `relay_first_18x` slice is not cleared on failover), so bob2's 200 reuses it.

use std::time::Duration;

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

/// Failover on reject: bob1 sends 180 then 503; the B2BUA fails over via
/// `/call/failure` to bob2 (new R-URI), which answers. Alice never sees the 503;
/// her 200 OK reuses the first 180's To-tag (continuity across the leg swap).
/// (TS `suppress18xFailoverReject`.)
#[tokio::test]
async fn failover_reject() {
    let h = Harness::with_transit_delay("suppress-18x-failover-reject", 0);
    let alice = h.agent("alice", "127.0.0.1:5603").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5613").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5614").await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        &h,
        "b2bua",
        "127.0.0.1:5623",
        "127.0.0.1",
        5613,
        5614,
        "sip:+1234@127.0.0.1:5614",
        RelayFirst18xStrategy::DropSdp,
    )
    .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;

    // Bob1 receives the INVITE, rings, then rejects.
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;

    let p180 = call.expect(180).await;
    let first_to_tag = p180.to.tag.clone().expect("180 has a To-tag");

    uas1.respond(503, "Service Unavailable").await;

    // Failover to bob2 (new R-URI per the on_failure decision).
    let mut uas2 = bob2.receive("INVITE").await;
    assert_eq!(uas2.request().uri, "sip:+1234@127.0.0.1:5614", "bob2 R-URI is the failover new_ruri");

    // Bob2's 18x are suppressed (alice already saw the bare 180 from bob1).
    uas2.respond(180, "Ringing").await;
    uas2.respond(180, "Ringing").await;

    // Bob2 answers; alice's 200 reuses the first 180's To-tag (from bob1!).
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(
        ok.to.tag.as_deref(),
        Some(first_to_tag.as_str()),
        "200 To-tag == first 180 To-tag across failover",
    );

    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Failover on no-answer: delayed-offer INVITE; bob1 rings then times out; the
/// B2BUA CANCELs bob1 and fails over via `/call/failure` to bob2 (new R-URI),
/// which answers with the offer. Alice's 200 reuses the first 180's To-tag.
/// (TS `suppress18xFailoverNoAnswer`.)
#[tokio::test(start_paused = true)]
async fn failover_no_answer() {
    let h = Harness::with_transit_delay("suppress-18x-failover-no-answer", 0);
    let alice = h.agent("alice", "127.0.0.1:5605").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5615").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5616").await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        &h,
        "b2bua",
        "127.0.0.1:5625",
        "127.0.0.1",
        5615,
        5616,
        "sip:+1234@127.0.0.1:5616",
        RelayFirst18xStrategy::DropSdp,
    )
    .await;

    // Delayed offer: no SDP in the INVITE — bob2's 200 OK carries the offer.
    let mut call = alice.invite(&bob1).through(b2bua.addr).send().await;

    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;

    let p180 = call.expect(180).await;
    let first_to_tag = p180.to.tag.clone().expect("180 has a To-tag");

    // No-answer timeout (30 s) → CANCEL bob1 + failover to bob2. Advance just
    // past the deadline (not a full second) so the failover INVITE to bob2 is
    // answered before its Timer A retransmit (~500 ms) leaks under the paused
    // clock — the deterministic equivalent of the TS `bob2.allowExtra("INVITE")`.
    h.advance(Duration::from_secs(30) + Duration::from_millis(100)).await;

    // Bob1 gets the CANCEL (tied to its still-open INVITE txn), 200s it, then
    // 487s the INVITE; the B2BUA auto-ACKs the 487.
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    uas1.respond(487, "Request Terminated").await;

    // Bob2 receives the failover INVITE with the configured new R-URI.
    let mut uas2 = bob2.receive("INVITE").await;
    assert_eq!(uas2.request().uri, "sip:+1234@127.0.0.1:5616", "bob2 R-URI is the failover new_ruri");

    // Bob2's 180 is suppressed; its 200 OK carries the (delayed) SDP offer.
    uas2.respond(180, "Ringing").await;
    uas2.respond(200, "OK").with_sdp(OFFER).await;

    let ok = call.expect(200).await;
    assert_eq!(
        ok.to.tag.as_deref(),
        Some(first_to_tag.as_str()),
        "200 To-tag == first 180 To-tag across failover",
    );

    // Alice answers the delayed offer in the ACK (RFC 3264 §4).
    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let ack = bob2.receive("ACK").await;
    assert!(!ack.request().body.is_empty(), "ACK carries the SDP answer");

    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
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
