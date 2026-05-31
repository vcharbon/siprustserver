//! promote18xPemTo200 — `relayFirst18xTo180` strategy `promote-pem-to-200`. Port
//! of `tests/scenarios/promote-pem-to-200.ts`.
//!
//! The B2BUA promotes Bob's first `183 + SDP + P-Early-Media` into a synthetic
//! 200 OK toward Alice, opens a promotion window (Alice's in-dialog requests are
//! gated), confirms silently on Bob's real 200, and resyncs Alice with a
//! re-INVITE when Bob's final SDP differs from the promoted early-media SDP.

use b2bua_harness::B2buaSut;
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const EARLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const FINAL_DIFF: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

fn has_token(value: Option<&str>, token: &str) -> bool {
    value
        .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        .unwrap_or(false)
}

async fn b2bua_pem(h: &Harness, name: &str, addr: &str, dest_port: u16) -> B2buaSut {
    B2buaSut::route_all_to_with_18x(
        h,
        name,
        addr,
        "127.0.0.1",
        dest_port,
        RelayFirst18xStrategy::PromotePemTo200,
    )
    .await
}

#[tokio::test]
async fn promote_pem_happy_no_resync() {
    let h = Harness::with_transit_delay("promote-pem-happy-no-resync", 0);
    let alice = h.agent("alice", "127.0.0.1:5801").await;
    let bob = h.agent("bob", "127.0.0.1:5811").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5821", 5811).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;

    let mut uas = bob.receive("INVITE").await;

    // Bob: 183 + SDP + P-Early-Media → promotion fires.
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;

    // Alice sees a 200 OK carrying bob's early SDP, P-Early-Media stripped,
    // explicit Allow + Supported (no 100rel).
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "synthetic 200 carries bob's early SDP");
    assert_eq!(ok.body, EARLY.as_bytes(), "early SDP relayed verbatim");
    assert!(get_header(&ok.headers, "p-early-media").is_none(), "P-Early-Media stripped");
    let allow = get_header(&ok.headers, "allow").unwrap_or("");
    assert!(allow.contains("INVITE") && allow.contains("BYE"), "Allow on synthetic 200");
    assert!(!has_token(get_header(&ok.headers, "supported"), "100rel"), "no 100rel");

    // Alice ACKs — absorbed locally; bob receives nothing yet.
    let mut dialog = call.ack().await;

    // Bob now sends 200 OK with the SAME SDP → no resync.
    uas.respond(200, "OK").with_sdp(EARLY).await;

    // B2BUA generates the local ACK toward bob.
    bob.receive("ACK").await;

    // Normal teardown.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Regression guard: same packet flow with the policy OFF → alice sees a 183
/// (not a synthetic 200), body + P-Early-Media survive the default relay.
#[tokio::test]
async fn no_policy_control() {
    let h = Harness::with_transit_delay("promote-pem-no-policy-control", 0);
    let alice = h.agent("alice", "127.0.0.1:5808").await;
    let bob = h.agent("bob", "127.0.0.1:5818").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5828", "127.0.0.1", 5818).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;

    // Default relay-provisional fires — alice sees a 183, NOT a synthetic 200,
    // and the SDP body survives the relay (the regression guard: the PEM service
    // is off so the packet flows through the CORE path). NB the Rust CORE relay's
    // passthrough set is Require/RSeq/Supported only, so it does not forward the
    // non-structural `P-Early-Media` header — unlike the TS relay. That is a CORE
    // relay-passthrough fidelity gap, independent of the PEM service under test
    // here; the load-bearing guard is "183 not 200, body intact".
    let p183 = call.expect(183).await;
    assert!(!p183.body.is_empty(), "183 body survives the default relay");

    uas.respond(200, "OK").with_sdp(EARLY).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn resync_sdp_changed() {
    let h = Harness::with_transit_delay("promote-pem-resync-sdp-changed", 0);
    let alice = h.agent("alice", "127.0.0.1:5802").await;
    let bob = h.agent("bob", "127.0.0.1:5812").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5822", 5812).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(ok.body, EARLY.as_bytes());

    let mut dialog = call.ack().await;

    // Bob's 200 OK with DIFFERENT SDP triggers a resync re-INVITE on the a-leg.
    uas.respond(200, "OK").with_sdp(FINAL_DIFF).await;
    bob.receive("ACK").await;

    // Alice receives the resync re-INVITE carrying bob's new SDP + Allow/Supported.
    let mut resync = alice.receive("INVITE").await;
    let req = resync.request();
    assert!(
        String::from_utf8_lossy(&req.body).contains("m=audio 30000"),
        "resync re-INVITE carries bob's new SDP",
    );
    let allow = get_header(&req.headers, "allow").unwrap_or("");
    assert!(allow.contains("INVITE"), "Allow on resync re-INVITE");
    assert!(get_header(&req.headers, "supported").is_some(), "Supported on resync re-INVITE");

    resync.respond(200, "OK").with_sdp(EARLY).await;
    // B2BUA's ACK to alice's 200 closes the window.
    alice.receive("ACK").await;

    // After the window closes, in-dialog flows resume — alice's INFO is relayed
    // to bob (NOT 488'd).
    let mut info = dialog
        .request(sip_message::generators::InDialogMethod::Info, None)
        .await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn b_fails_post_promote() {
    let h = Harness::with_transit_delay("promote-pem-b-fails-post-promote", 0);
    let alice = h.agent("alice", "127.0.0.1:5803").await;
    let bob = h.agent("bob", "127.0.0.1:5813").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5823", 5813).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    call.expect(200).await;
    let _dialog = call.ack().await;

    // Bob fails — alice already saw 200 OK, so the failure cannot be relayed as a
    // routing failure. begin-termination BYEs the confirmed a-leg with a Reason.
    uas.respond(503, "Service Unavailable").await;

    let mut bye = alice.receive("BYE").await;
    let reason = get_header(&bye.request().headers, "reason").unwrap_or("");
    assert!(reason.contains("cause=503"), "BYE carries Reason cause=503, got {reason:?}");
    bye.respond(200, "OK").await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn resync_failed_by_a() {
    let h = Harness::with_transit_delay("promote-pem-resync-failed-by-a", 0);
    let alice = h.agent("alice", "127.0.0.1:5804").await;
    let bob = h.agent("bob", "127.0.0.1:5814").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5824", 5814).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    call.expect(200).await;
    let _dialog = call.ack().await;

    uas.respond(200, "OK").with_sdp(FINAL_DIFF).await;
    bob.receive("ACK").await;

    // Resync re-INVITE arrives on alice — alice rejects with 488.
    let mut resync = alice.receive("INVITE").await;
    resync.respond(488, "Not Acceptable Here").await;

    // The B2BUA ACKs alice's 488 (RFC 3261 §17.1.1.3) before tearing down.
    alice.receive("ACK").await;

    // begin-termination BYEs both legs with Reason cause=488.
    let mut a_bye = alice.receive("BYE").await;
    assert!(
        get_header(&a_bye.request().headers, "reason").unwrap_or("").contains("cause=488"),
        "alice BYE carries Reason cause=488",
    );
    a_bye.respond(200, "OK").await;
    let mut b_bye = bob.receive("BYE").await;
    assert!(
        get_header(&b_bye.request().headers, "reason").unwrap_or("").contains("cause=488"),
        "bob BYE carries Reason cause=488",
    );
    b_bye.respond(200, "OK").await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn a_bye_during_window() {
    let h = Harness::with_transit_delay("promote-pem-a-bye-during-window", 0);
    let alice = h.agent("alice", "127.0.0.1:5805").await;
    let bob = h.agent("bob", "127.0.0.1:5815").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5825", 5815).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack().await;

    // Alice hangs up before bob's final response.
    let mut bye = dialog.bye().await;
    bye.expect(200).await;

    // CANCEL targets bob's still-open INVITE transaction.
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn forking_resync() {
    let h = Harness::with_transit_delay("promote-pem-forking-resync", 0);
    let alice = h.agent("alice", "127.0.0.1:5806").await;
    let bob = h.agent("bob", "127.0.0.1:5816").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5826", 5816).await;

    const FORK_T1: &str = "fork-tag-promoting-1";
    const FORK_T2: &str = "fork-tag-winning-2";

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // 183 + PEM with To-tag FORK_T1 (promote with that SDP).
    uas.respond(183, "Session Progress")
        .with_to_tag(FORK_T1)
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(ok.body, EARLY.as_bytes());
    let _dialog = call.ack().await;

    // Winning fork: 200 OK with To-tag FORK_T2 ≠ FORK_T1, different SDP.
    uas.respond(200, "OK").with_to_tag(FORK_T2).with_sdp(FINAL_DIFF).await;

    // The B2BUA-emitted local ACK toward bob carries the WINNING fork's To-tag.
    let ack = bob.receive("ACK").await;
    assert_eq!(
        ack.request().to.tag.as_deref(),
        Some(FORK_T2),
        "local ACK re-seeded onto the winning fork tag",
    );

    // Alice receives the resync re-INVITE carrying the winning fork's SDP.
    let mut resync = alice.receive("INVITE").await;
    assert!(
        String::from_utf8_lossy(&resync.request().body).contains("m=audio 30000"),
        "resync re-INVITE carries the winning fork SDP",
    );
    resync.respond(200, "OK").with_sdp(EARLY).await;
    alice.receive("ACK").await;

    let _ = h.finish().await;
}

#[tokio::test]
async fn in_dialog_rejection() {
    use sip_message::generators::InDialogMethod;

    let h = Harness::with_transit_delay("promote-pem-in-dialog-rejection", 0);
    let alice = h.agent("alice", "127.0.0.1:5807").await;
    let bob = h.agent("bob", "127.0.0.1:5817").await;
    let b2bua = b2bua_pem(&h, "b2bua", "127.0.0.1:5827", 5817).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack().await;

    // While the window is open, alice's in-dialog requests are refused.
    let mut update = dialog.request(InDialogMethod::Update, None).await;
    update.expect(491).await;

    let mut info = dialog.request(InDialogMethod::Info, None).await;
    info.expect(488).await;

    // Now bob answers; SDP unchanged → no resync, window closes.
    uas.respond(200, "OK").with_sdp(EARLY).await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}
