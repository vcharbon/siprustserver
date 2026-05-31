//! REFER gating scenarios (slice 5d). Port of `tests/scenarios/refer-gating.ts`.
//!
//! Verification slice for the two gating regimes (slice5-refer-design.md §5):
//!
//!   - Regime 1 — transparency (`refer-authorizing`, `c-ringing`): no transfer
//!     rule gates in-dialog A↔B traffic, so an A re-INVITE / A INFO / B INFO
//!     relays end-to-end via the CORE relay rules. The transfer slice is present
//!     but its request matchers only fire in the realigning phases, so they
//!     decline and CORE relays.
//!   - Regime 2 — rejection (`c-realigning`, `a-realigning`): a foreign A
//!     re-INVITE glares → 491 (`transfer-a-glare-reinvite`). A second REFER in
//!     any active phase → 491 (`transfer-reject-second-refer`).
//!
//! Rows covered elsewhere and skipped here (per the TS header comment):
//!   - A re-INVITE during a-realigning → 491   (refer_full_transfer.rs case 3)
//!   - B non-BYE during c-realigning → 481     (refer_c_realign.rs case 5)
//!   - Second REFER during refer-authorizing → 491 (refer-reject family)

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
// A's re-INVITE offer (a distinct media port so the relayed body is recognisable).
const AREINVITE: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const AREINVITE_ANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const DTMF: &str = "Signal=5\r\nDuration=160\r\n";

const CHARLIE_PORT: u16 = 5667;

fn x_api_allow_c() -> String {
    format!(r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#)
}

fn x_api_http_timeout() -> String {
    r#"{"refer_key":"refer-http-timeout"}"#.to_string()
}

fn refer_to_charlie() -> String {
    format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>")
}

/// Assert a received NOTIFY carries `Event: refer`, a `Subscription-State`
/// starting with `prefix`, and a sipfrag body containing `frag`.
fn assert_notify(txn: &ServerTxn, prefix: &str, frag: &str) {
    let req = txn.request();
    assert_eq!(req.method, "NOTIFY", "expected NOTIFY");
    assert_eq!(get_header(&req.headers, "event").unwrap_or(""), "refer", "NOTIFY Event: refer");
    let ss = get_header(&req.headers, "subscription-state").unwrap_or("");
    assert!(ss.starts_with(prefix), "subscription-state {ss:?} should start with {prefix:?}");
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains(frag), "sipfrag body {body:?} should contain {frag:?}");
}

fn assert_reinvite(req: &sip_message::SipRequest, body: &str, leg: &str) {
    assert_eq!(req.method, "INVITE", "expected re-INVITE");
    assert!(req.cseq.seq > 1, "re-INVITE CSeq.seq {} should be > 1", req.cseq.seq);
    let contact = get_header(&req.headers, "contact").unwrap_or("");
    assert!(contact.contains(&format!("leg={leg}")), "Contact {contact:?} should carry leg={leg}");
    let _ = body;
}

// ── 1. A re-INVITE during refer-authorizing → transparent relay to B ──────

#[tokio::test(start_paused = true)]
async fn refer_gating_a_reinvite_refer_authorizing() {
    let h = Harness::new("refer-gating-a-reinvite-refer-authorizing");
    let alice = h.agent("alice", "127.0.0.1:6001").await;
    let bob = h.agent("bob", "127.0.0.1:6011").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6021", "127.0.0.1", 6011).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // REFER whose HTTP authorization hangs → pinned in refer-authorizing for 60s.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_http_timeout())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // A re-INVITEs mid-authorising — must relay transparently to B.
    let mut a_reinvite = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(AREINVITE)
        .send()
        .await;
    let mut bob_reinvite = bob.receive("INVITE").await;
    assert_eq!(
        String::from_utf8_lossy(&bob_reinvite.request().body),
        AREINVITE,
        "A's re-INVITE body relayed verbatim to B"
    );
    bob_reinvite.respond(200, "OK").with_sdp(AREINVITE_ANSWER).await;
    a_reinvite.expect(200).await;
    alice_dialog.ack(Some(AREINVITE_ANSWER)).await;
    bob.receive("ACK").await;

    // Advance toward the 60s subscription-expiry. The 30s keepalive cycle fires
    // first; answer the OPTIONS on both legs to keep the call up.
    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // Cross the 60s sub-expiry → NOTIFY 500 terminated on B. The 60s keepalive
    // OPTIONS fires in the same advance (and a cycle-1 retransmit may still be in
    // flight); receive B's NOTIFY tolerating (auto-answering) B's OPTIONS.
    h.advance(Duration::from_secs(30)).await;
    let mut nterm = bob.receive_tolerating("NOTIFY", &["OPTIONS"]).await;
    assert_notify(&nterm, "terminated", "SIP/2.0 500");
    nterm.respond(200, "OK").await;

    // A↔B intact (the transfer resolved at sub-expiry); tear down via A BYE.
    // Tolerate keepalive OPTIONS retransmits racing the BYE 200 on either leg.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY", "OPTIONS"]).await.respond(200, "OK").await;
    alice_bye.expect_tolerating(200, &["OPTIONS"]).await;

    let _ = h.finish().await;
}

// ── 2. A re-INVITE during c-ringing → transparent relay to B ──────────────

#[tokio::test]
async fn refer_gating_a_reinvite_c_ringing() {
    let h = Harness::with_transit_delay("refer-gating-a-reinvite-c-ringing", 1);
    let alice = h.agent("alice", "127.0.0.1:6002").await;
    let bob = h.agent("bob", "127.0.0.1:6012").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6022", "127.0.0.1", 6012).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // REFER allowed → C INVITE → 180 pins us in c-ringing.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(180, "Ringing").await;
    let mut n180 = bob.receive("NOTIFY").await;
    assert_notify(&n180, "active", "SIP/2.0 180");
    n180.respond(200, "OK").await;

    // A re-INVITEs mid c-ringing — must still relay transparently to B.
    let mut a_reinvite = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(AREINVITE)
        .send()
        .await;
    let mut bob_reinvite = bob.receive("INVITE").await;
    assert_eq!(
        String::from_utf8_lossy(&bob_reinvite.request().body),
        AREINVITE,
        "A's re-INVITE body relayed verbatim to B"
    );
    bob_reinvite.respond(200, "OK").with_sdp(AREINVITE_ANSWER).await;
    a_reinvite.expect(200).await;
    alice_dialog.ack(Some(AREINVITE_ANSWER)).await;
    bob.receive("ACK").await;

    // C rejects 486 → NOTIFY 486 terminated → transfer cleared, A↔B alive.
    charlie_uas.respond(486, "Busy Here").await;
    charlie.receive("ACK").await;
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 486");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 3. A re-INVITE during c-realigning → 491, then resume to completion ───

#[tokio::test]
async fn refer_gating_a_reinvite_c_realigning() {
    let h = Harness::with_transit_delay("refer-gating-a-reinvite-c-realigning", 1);
    let alice = h.agent("alice", "127.0.0.1:6003").await;
    let bob = h.agent("bob", "127.0.0.1:6013").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6023", "127.0.0.1", 6013).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // C answers its initial INVITE 200; B2BUA ACKs.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    let mut charlie_dialog = charlie_uas.dialog();

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // Observe the c-realign re-INVITE toward C — pins us in c-realigning.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");

    // A glares during c-realigning — transfer-a-glare-reinvite returns 491.
    let mut a_glare = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(AREINVITE)
        .send()
        .await;
    a_glare.expect(491).await;

    // Resume the c-realign exchange → a-realign re-INVITE toward A.
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");
    a_realign.respond(200, "OK").with_sdp(ANSWER).await;
    alice.receive("ACK").await;

    // Merge complete — A BYE tears down both peers.
    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = &mut charlie_dialog;
    let _ = h.finish().await;
}

// ── 4. A INFO during refer-authorizing → transparent relay to B ───────────

#[tokio::test(start_paused = true)]
async fn refer_gating_a_info_refer_authorizing() {
    let h = Harness::new("refer-gating-a-info-refer-authorizing");
    let alice = h.agent("alice", "127.0.0.1:6004").await;
    let bob = h.agent("bob", "127.0.0.1:6014").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6024", "127.0.0.1", 6014).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_http_timeout())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // A INFO during refer-authorizing → relays to B (relay-info).
    let mut a_info = alice_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/dtmf-relay")
        .with_sdp(DTMF)
        .send()
        .await;
    let mut bob_info = bob.receive("INFO").await;
    assert_eq!(
        get_header(&bob_info.request().headers, "content-type").unwrap_or(""),
        "application/dtmf-relay",
        "INFO Content-Type relayed"
    );
    assert_eq!(
        String::from_utf8_lossy(&bob_info.request().body),
        DTMF,
        "INFO body relayed verbatim to B"
    );
    bob_info.respond(200, "OK").await;
    a_info.expect(200).await;

    // Advance to the 60s sub-expiry, answering the 30s keepalive cycle first.
    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    h.advance(Duration::from_secs(30)).await;
    let mut nterm = bob.receive_tolerating("NOTIFY", &["OPTIONS"]).await;
    assert_notify(&nterm, "terminated", "SIP/2.0 500");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY", "OPTIONS"]).await.respond(200, "OK").await;
    alice_bye.expect_tolerating(200, &["OPTIONS"]).await;

    let _ = h.finish().await;
}

// ── 5. A INFO during c-ringing → transparent relay to B ───────────────────

#[tokio::test]
async fn refer_gating_a_info_c_ringing() {
    let h = Harness::with_transit_delay("refer-gating-a-info-c-ringing", 1);
    let alice = h.agent("alice", "127.0.0.1:6005").await;
    let bob = h.agent("bob", "127.0.0.1:6015").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6025", "127.0.0.1", 6015).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(180, "Ringing").await;
    let mut n180 = bob.receive("NOTIFY").await;
    assert_notify(&n180, "active", "SIP/2.0 180");
    n180.respond(200, "OK").await;

    // A INFO during c-ringing → relays to B.
    let mut a_info = alice_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/dtmf-relay")
        .with_sdp(DTMF)
        .send()
        .await;
    let mut bob_info = bob.receive("INFO").await;
    assert_eq!(
        String::from_utf8_lossy(&bob_info.request().body),
        DTMF,
        "INFO body relayed verbatim to B"
    );
    bob_info.respond(200, "OK").await;
    a_info.expect(200).await;

    // C rejects 486 → NOTIFY 486 terminated → transfer cleared, A↔B alive.
    charlie_uas.respond(486, "Busy Here").await;
    charlie.receive("ACK").await;
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 486");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 6. B INFO during refer-authorizing → transparent relay to A ───────────

#[tokio::test(start_paused = true)]
async fn refer_gating_b_info_refer_authorizing() {
    let h = Harness::new("refer-gating-b-info-refer-authorizing");
    let alice = h.agent("alice", "127.0.0.1:6006").await;
    let bob = h.agent("bob", "127.0.0.1:6016").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6026", "127.0.0.1", 6016).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_http_timeout())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // B INFO during refer-authorizing → relays to A (no transfer rule gates it).
    let mut b_info = bob_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/dtmf-relay")
        .with_sdp(DTMF)
        .send()
        .await;
    let mut alice_info = alice.receive("INFO").await;
    assert_eq!(
        String::from_utf8_lossy(&alice_info.request().body),
        DTMF,
        "INFO body relayed verbatim to A"
    );
    alice_info.respond(200, "OK").await;
    b_info.expect(200).await;

    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    h.advance(Duration::from_secs(30)).await;
    let mut nterm = bob.receive_tolerating("NOTIFY", &["OPTIONS"]).await;
    assert_notify(&nterm, "terminated", "SIP/2.0 500");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY", "OPTIONS"]).await.respond(200, "OK").await;
    alice_bye.expect_tolerating(200, &["OPTIONS"]).await;

    let _ = h.finish().await;
}

// ── 7. Second REFER during c-ringing → 491 ────────────────────────────────

#[tokio::test]
async fn refer_gating_second_refer_c_ringing() {
    let h = Harness::with_transit_delay("refer-gating-second-refer-c-ringing", 1);
    let alice = h.agent("alice", "127.0.0.1:6007").await;
    let bob = h.agent("bob", "127.0.0.1:6017").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6027", "127.0.0.1", 6017).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(180, "Ringing").await;
    let mut n180 = bob.receive("NOTIFY").await;
    assert_notify(&n180, "active", "SIP/2.0 180");
    n180.respond(200, "OK").await;

    // Second REFER while c-ringing → 491 (transfer-reject-second-refer).
    let mut second_refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .send()
        .await;
    second_refer.expect(491).await;

    // Resolve the first REFER cleanly (C 486).
    charlie_uas.respond(486, "Busy Here").await;
    charlie.receive("ACK").await;
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 486");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 8. Second REFER during c-realigning → 491 ─────────────────────────────

#[tokio::test]
async fn refer_gating_second_refer_c_realigning() {
    let h = Harness::with_transit_delay("refer-gating-second-refer-c-realigning", 1);
    let alice = h.agent("alice", "127.0.0.1:6008").await;
    let bob = h.agent("bob", "127.0.0.1:6018").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:6028", "127.0.0.1", 6018).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    let mut charlie_dialog = charlie_uas.dialog();

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // Observe the c-realign re-INVITE → pinned in c-realigning.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");

    // Second REFER mid c-realigning → 491 (transfer-reject-second-refer).
    let mut second_refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .send()
        .await;
    second_refer.expect(491).await;

    // Resume and complete the transfer.
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");
    a_realign.respond(200, "OK").with_sdp(ANSWER).await;
    alice.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = &mut charlie_dialog;
    let _ = h.finish().await;
}
