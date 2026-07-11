//! fake-prack — `relayFirst18xTo180` strategy `fake-prack`. Port of
//! `tests/scenarios/fake-prack.ts`.
//!
//! The B2BUA keeps bob on reliable provisional (Supported:100rel forwarded),
//! downgrades the first 18x to a bare 180 for alice, **originates** the PRACK
//! toward bob itself, caches bob's reliable-1xx SDP per dialog, and substitutes
//! the cached SDP into the 200 OK toward alice. Locally answers in-dialog UPDATE
//! (skeleton-fit SDP from alice's offer, else 488).
//!
//! The `forking` / `failover` cases ride the `/call/failure` b-leg failover path:
//! bob1 goes reliable (183/100rel + PRACK + cached SDP) then 503s; the B2BUA fails
//! over to bob2 on the unreliable path. bob1's cache is discarded with its leg, so
//! alice's 200 carries bob2's own SDP.

use b2bua_harness::B2buaSut;
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8 18 101\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

fn has_token(value: Option<&str>, token: &str) -> bool {
    value
        .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        .unwrap_or(false)
}

fn rack_matches(headers: &[sip_message::SipHeader], rseq: i64, method: &str) -> bool {
    get_header(headers, "rack")
        .map(|r| {
            let p: Vec<&str> = r.split_whitespace().collect();
            p.len() == 3
                && p[0] == rseq.to_string()
                && p[2].eq_ignore_ascii_case(method)
        })
        .unwrap_or(false)
}

async fn b2bua_fake_prack(h: &Harness, name: &str, addr: &str, dest_port: u16) -> B2buaSut {
    B2buaSut::route_all_to_with_18x("127.0.0.1", dest_port, RelayFirst18xStrategy::FakePrack)
        .start(h, name, addr)
        .await
}

#[tokio::test]
async fn basic() {
    let h = Harness::with_transit_delay("fake-prack-basic", 0);
    let alice = h.agent("alice", "127.0.0.1:5701").await;
    let bob = h.agent("bob", "127.0.0.1:5711").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5721", 5711).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, timer")
        .through(b2bua.addr)
        .send()
        .await;

    // fake-prack KEEPS Supported:100rel toward bob (so he goes reliable).
    let mut uas = bob.receive("INVITE").await;
    assert!(
        has_token(get_header(&uas.request().headers, "supported"), "100rel"),
        "100rel kept in bob's Supported",
    );

    // Bob: 183 with Require:100rel + RSeq + SDP.
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;

    // Alice sees a bare 180.
    let p180 = call.expect(180).await;
    assert!(p180.body.is_empty(), "bare 180, no body");
    assert!(!has_token(get_header(&p180.headers, "require"), "100rel"));
    assert!(get_header(&p180.headers, "rseq").is_none());

    // B2BUA originates the PRACK toward bob.
    let mut prack = bob.receive("PRACK").await;
    assert!(rack_matches(&prack.request().headers, 1, "INVITE"), "RAck 1 .. INVITE");
    prack.respond(200, "OK").await;

    // Bob's 200 OK has NO body — alice's 200 carries the cached 18x SDP.
    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "alice 200 carries cached SDP");
    assert!(
        get_header(&ok.headers, "content-type")
            .map(|c| c.to_ascii_lowercase().contains("application/sdp"))
            .unwrap_or(false),
        "Content-Type application/sdp on alice's 200",
    );

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn multiple_18x() {
    let h = Harness::with_transit_delay("fake-prack-multiple-18x", 0);
    let alice = h.agent("alice", "127.0.0.1:5702").await;
    let bob = h.agent("bob", "127.0.0.1:5712").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5722", 5712).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // 183 (RSeq 1) with SDP #1.
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    let mut p1 = bob.receive("PRACK").await;
    assert!(rack_matches(&p1.request().headers, 1, "INVITE"));
    p1.respond(200, "OK").await;

    // 180 (RSeq 2) with SDP #2 — suppressed for alice, PRACKed + re-cached.
    uas.respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", "2")
        .with_sdp(ANSWER)
        .await;
    let mut p2 = bob.receive("PRACK").await;
    assert!(rack_matches(&p2.request().headers, 2, "INVITE"));
    p2.respond(200, "OK").await;

    // 200 OK with no body → alice gets the latest cached SDP.
    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "alice 200 carries cached SDP");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

const OPUS_ONLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 96\r\na=rtpmap:96 opus/48000/2\r\na=sendrecv\r\n";

/// Bob's UPDATE offers only a codec alice never offered → no intersection →
/// B2BUA replies 488; the call proceeds on the original cached SDP from the 183.
/// (TS `fakePrackUpdateCodecMismatch`.)
#[tokio::test]
async fn update_codec_mismatch() {
    let h = Harness::with_transit_delay("fake-prack-update-codec-mismatch", 0);
    let alice = h.agent("alice", "127.0.0.1:5704").await;
    let bob = h.agent("bob", "127.0.0.1:5714").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5724", 5714).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
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

    // UPDATE with no codec overlap → 488.
    let mut bob_dialog = uas.dialog();
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(OPUS_ONLY)).await;
    update.expect(488).await;

    // Call still proceeds on the original cached SDP from the 183.
    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "alice 200 carries the original cached SDP");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// No policy → full end-to-end reliable provisional: bob's 183(100rel) is
/// relayed verbatim (Require:100rel intact), alice PRACKs end-to-end. Regression
/// guard that the SERVICE_LAYER path stays off the default flow. (TS
/// `fakePrackNoPolicyControl`.)
#[tokio::test]
async fn no_policy_control() {
    let h = Harness::with_transit_delay("fake-prack-no-policy-control", 0);
    let alice = h.agent("alice", "127.0.0.1:5706").await;
    let bob = h.agent("bob", "127.0.0.1:5716").await;
    // route_all_to → no relay_first_18x feature.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5716).start(&h, "b2bua", "127.0.0.1:5726").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // Bob 183 with 100rel + SDP — relayed end-to-end with Require:100rel intact.
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    let p183 = call.expect(183).await;
    assert!(
        has_token(get_header(&p183.headers, "require"), "100rel"),
        "Require:100rel relayed verbatim (default path)",
    );

    // Alice PRACKs end-to-end; the B2BUA relays it to bob.
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_rack("1 1 INVITE")
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Alice's INVITE has no SDP (delayed offer) → the outbound INVITE strips
/// `Supported:100rel` and the policy self-disables (plain relay; bob uses
/// unreliable provisional, offer in his 200, answer in alice's ACK). (TS
/// `fakePrackDelayedOfferFallback`.)
#[tokio::test]
async fn delayed_offer_fallback() {
    let h = Harness::with_transit_delay("fake-prack-delayed-offer-fallback", 0);
    let alice = h.agent("alice", "127.0.0.1:5705").await;
    let bob = h.agent("bob", "127.0.0.1:5715").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5725", 5715).await;

    // Alice INVITE with NO body — delayed offer.
    let mut call = alice
        .invite(&bob)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // Outbound INVITE to bob must have Supported:100rel stripped.
    let mut uas = bob.receive("INVITE").await;
    assert!(
        !has_token(get_header(&uas.request().headers, "supported"), "100rel"),
        "100rel stripped on the delayed-offer fallback",
    );

    // Bob uses unreliable provisional: a plain 180 relayed normally to alice
    // (policy self-disabled → no bare-180 downgrade beyond the plain relay).
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Bob's 200 carries the offer (delayed-offer model).
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // Alice answers in the ACK.
    let mut dialog = call.ack_with(Some(OFFER)).await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Bob's early-dialog UPDATE(offer) is answered locally by the B2BUA with a
/// skeleton-fit SDP (codec ∩ alice's INVITE), and the cache advances to bob's
/// UPDATE offer (alice's 200 carries it). (TS `fakePrackUpdateHappy`.)
#[tokio::test]
async fn update_happy() {
    let h = Harness::with_transit_delay("fake-prack-update-happy", 0);
    let alice = h.agent("alice", "127.0.0.1:5703").await;
    let bob = h.agent("bob", "127.0.0.1:5713").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5723", 5713).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
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

    // Bob sends an UPDATE with a new offer on his early dialog.
    let mut bob_dialog = uas.dialog();
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(ANSWER)).await;
    // B2BUA answers locally: 200 with a skeleton-fit SDP body.
    let upd_resp = update.expect(200).await;
    assert!(!upd_resp.body.is_empty(), "skeleton-fit answer has a body");
    assert!(
        get_header(&upd_resp.headers, "content-type")
            .map(|c| c.to_ascii_lowercase().contains("application/sdp"))
            .unwrap_or(false),
        "Content-Type application/sdp on the local UPDATE answer",
    );

    // 200 OK INVITE (no body) → alice gets the latest cached SDP (UPDATE offer).
    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "alice 200 carries cached SDP");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Shared body for the two failover-on-503 cases (TS `forking` / `failover`):
/// bob1 goes reliable (183/100rel → bare 180 + B2BUA PRACK + cached SDP) then
/// 503s; the B2BUA fails over to bob2 (unreliable). bob1's cache dies with its
/// leg, so alice's 200 carries bob2's own SDP.
async fn run_fake_prack_failover(scenario: &str, alice_p: u16, bob1_p: u16, bob2_p: u16, b2b_p: u16) {
    let h = Harness::with_transit_delay(scenario, 0);
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_p}")).await;
    let bob1 = h.agent("bob1", &format!("127.0.0.1:{bob1_p}")).await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{bob2_p}")).await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        "127.0.0.1",
        bob1_p,
        bob2_p,
        &format!("sip:+1234@127.0.0.1:{bob2_p}"),
        RelayFirst18xStrategy::FakePrack,
    )
    .start(&h, "b2bua", &format!("127.0.0.1:{b2b_p}"))
    .await;

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // bob1: reliable 183 → bare 180 to alice + B2BUA-originated PRACK.
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    let mut prack = bob1.receive("PRACK").await;
    assert!(rack_matches(&prack.request().headers, 1, "INVITE"), "RAck 1 .. INVITE");
    prack.respond(200, "OK").await;

    // bob1 rejects → failover to bob2.
    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await; // the b2bua completes bob1's reject txn (§17.1.1.3)

    // bob2 on the unreliable path: 180 suppressed, 200 with its own SDP.
    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(180, "Ringing").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;

    // alice's 200 carries bob2's SDP (bob1's cache discarded with its leg).
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "alice 200 carries bob2's SDP");

    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn forking() {
    run_fake_prack_failover("fake-prack-forking", 5750, 5751, 5752, 5753).await;
}

#[tokio::test]
async fn failover() {
    run_fake_prack_failover("fake-prack-failover", 5740, 5741, 5742, 5743).await;
}
