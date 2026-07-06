//! Full-coverage matrix for RFC 3311 UPDATE across every dialog state the
//! B2BUA must relay it in — the gap-hunt behind "UPDATE must work in all cases,
//! established and early, PRACKed or not, with or without SDP, including
//! forking". The happy 2xx-relay paths for a *PRACKed* early UPDATE and a
//! confirmed UPDATE (both WITH SDP) are already pinned by
//! `prack_update_forking.rs`; this file fills the untested cells:
//!
//!   • established UPDATE **without** SDP (session-timer-style refresh), A→B and B→A
//!   • early **not-PRACKed** UPDATE relayed A→B (with and without SDP)
//!   • early **not-PRACKed** UPDATE B→A (callee adjusts early media, RFC 3311 §5.1)
//!   • early **PRACKed** UPDATE without SDP, A→B
//!   • early UPDATE on a **non-first fork** without SDP
//!   • early UPDATE **B→A from a non-first fork** (source-dialog correlation)
//!
//! Everything routes the CORE transparent path (`route_all_to`), so the relay,
//! per-dialog CSeq (RFC 3261 §12.2.1.1) and response-correlation (§8.1.3.3) are
//! exercised end-to-end and hard-gated by the harness RFC audit at `finish()`.

use b2bua_harness::{B2buaScene, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const REOFFER_HOLD: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";

// ── Established dialog: UPDATE WITHOUT SDP (session refresh) ──────────────────

/// Confirmed dialog, alice → UPDATE with **no body** (RFC 4028-style session
/// refresh). The bodyless UPDATE must relay to bob verbatim and its 200 come
/// back — the "UPDATE without SDP" cell the SDP-carrying tests never touch.
#[tokio::test]
async fn established_update_no_sdp_a_to_b() {
    let s = B2buaScene::new("upd-established-nosdp-a2b").await;
    let mut dialog = s.establish().await;

    let mut update = dialog.request(InDialogMethod::Update, None).await;
    let mut at_bob = s.bob.receive("UPDATE").await;
    assert!(at_bob.request().body.is_empty(), "bodyless UPDATE relayed with no body");
    at_bob.respond(200, "OK").await;
    update.expect(200).await;

    s.hangup(&mut dialog).await;
    let _ = s.finish().await;
}

/// Same, callee-initiated: bob → UPDATE with no body on the established dialog.
#[tokio::test]
async fn established_update_no_sdp_b_to_a() {
    let h = Harness::new("upd-established-nosdp-b2a");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut update = bob_dialog.request(InDialogMethod::Update, None).await;
    let mut at_alice = alice.receive("UPDATE").await;
    assert!(at_alice.request().body.is_empty(), "bodyless UPDATE relayed to alice");
    at_alice.respond(200, "OK").await;
    update.expect(200).await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

// ── Early dialog, NOT PRACKed (plain 180): UPDATE relayed A→B ─────────────────

/// Plain 180 (unreliable) → early dialog. Alice sends an early-dialog UPDATE
/// (RFC 3311 §5.1) that must relay to bob even though no PRACK ever happened.
/// Runs with and without SDP.
async fn early_not_pracked_a_to_b(name: &str, alice_port: &str, bob_port_n: u16, b2bua_port: &str, with_sdp: bool) {
    let h = Harness::new(name);
    let alice = h.agent("alice", alice_port).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port_n}")).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob_port_n).start(&h, "b2bua", b2bua_port).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    // Unreliable 180 with a To-tag: establishes the early dialog + a-facing tag,
    // no 100rel, no PRACK.
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let atag = ringing.to.tag.clone().expect("a-facing early tag on the 180");

    // Alice UPDATEs the early dialog (not PRACKed).
    let mut ub = call.send_request(InDialogMethod::Update).with_to_tag(&atag);
    if with_sdp {
        ub = ub.with_sdp(REOFFER_HOLD);
    }
    let mut update = ub.send().await;
    let mut at_bob = bob.receive("UPDATE").await;
    assert_eq!(at_bob.request().body.is_empty(), !with_sdp, "body presence relayed faithfully");
    if with_sdp {
        at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    } else {
        at_bob.respond(200, "OK").await;
    }
    update.expect(200).await;

    // Answer (reusing the 180's To-tag → the early dialog is confirmed) + teardown.
    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

#[tokio::test]
async fn early_not_pracked_update_a_to_b_with_sdp() {
    early_not_pracked_a_to_b("upd-early-nopr-a2b-sdp", "127.0.0.1:5061", 5071, "127.0.0.1:5081", true).await;
}

#[tokio::test]
async fn early_not_pracked_update_a_to_b_no_sdp() {
    early_not_pracked_a_to_b("upd-early-nopr-a2b-nosdp", "127.0.0.1:5062", 5072, "127.0.0.1:5082", false).await;
}

// ── Early dialog, NOT PRACKed: UPDATE B→A (callee early-media adjust) ─────────

/// Callee adjusts early media before answering: bob sends an UPDATE on the
/// **early** dialog toward alice (RFC 3311 §5.1). Must relay A-ward and the
/// 200 come back to bob. Not PRACKed, with SDP.
#[tokio::test]
async fn early_not_pracked_update_b_to_a() {
    let h = Harness::new("upd-early-nopr-b2a");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_sdp(ANSWER).await; // early media + auto To-tag
    call.expect(180).await;

    // bob's early dialog (local_tag = the auto-minted 180 To-tag).
    let mut bob_dialog = uas.dialog();
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(REOFFER_HOLD)).await;
    let mut at_alice = alice.receive("UPDATE").await;
    assert!(
        String::from_utf8_lossy(&at_alice.request().body).contains("a=sendonly"),
        "callee early-media re-offer relayed to alice",
    );
    at_alice.respond(200, "OK").with_sdp(OFFER).await;
    update.expect(200).await;

    // answer + teardown
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

// ── Early dialog, PRACKed: UPDATE WITHOUT SDP, A→B ───────────────────────────

/// Reliable 183 + PRACK, then an early-dialog UPDATE with **no body**. Pins the
/// "PRACKed early dialog, bodyless UPDATE" cell.
#[tokio::test]
async fn early_pracked_update_no_sdp_a_to_b() {
    let h = Harness::new("upd-early-pracked-nosdp-a2b");
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let atag = p183.to.tag.clone().expect("a-facing tag");

    // PRACK the reliable 183.
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&atag)
        .with_rack("1 1 INVITE")
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    // Bodyless early UPDATE.
    let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&atag).send().await;
    let mut at_bob = bob.receive("UPDATE").await;
    assert!(at_bob.request().body.is_empty(), "bodyless early UPDATE relayed");
    at_bob.respond(200, "OK").await;
    update.expect(200).await;

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

// ── Forking: early UPDATE (no SDP) on a non-first fork, A→B ───────────────────

/// Two early dialogs (plain, unreliable 180s on distinct callee tags — a proxy
/// fork); alice UPDATEs the **second** fork with no body. The bodyless UPDATE
/// must ride fork 2's own dialog + CSeq.
#[tokio::test]
async fn early_update_forking_no_sdp_second_fork() {
    let h = Harness::with_transit_delay("upd-fork-nosdp-f2", 1);
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(180, "Ringing").with_to_tag("bf1").await;
    let p1 = call.expect(180).await;
    let f1 = p1.to.tag.clone().expect("fork1 a-tag");

    uas.respond(180, "Ringing").with_to_tag("bf2").await;
    let p2 = call.expect(180).await;
    let f2 = p2.to.tag.clone().expect("fork2 a-tag");
    assert_ne!(f1, f2);

    // Bodyless UPDATE on fork 2.
    let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&f2).send().await;
    let mut at_bob = bob.receive("UPDATE").await;
    assert_eq!(at_bob.request().to.tag.as_deref(), Some("bf2"), "UPDATE rode fork 2");
    assert!(at_bob.request().body.is_empty());
    at_bob.respond(200, "OK").await;
    update.expect(200).await;

    // Answer on fork 2 with the SDP answer, teardown.
    uas.respond(200, "OK").with_to_tag("bf2").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to.tag.as_deref(), Some(f2.as_str()));
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

// ── Forking: early UPDATE B→A from a non-first fork ───────────────────────────

/// A downstream fork (fork 2) sends an early-dialog UPDATE toward the caller.
/// The B2BUA must correlate it to fork 2's source dialog (its own remote-CSeq
/// bookkeeping and pending-relay snapshot), not fork 1's — the
/// source-dialog-by-From-tag case. Fork 1 gets an explicit tag; fork 2 uses the
/// harness's auto-minted (sticky) tag so `uas.dialog()` originates on fork 2.
#[tokio::test]
async fn early_update_forking_b_to_a_second_fork() {
    let h = Harness::with_transit_delay("upd-fork-b2a-f2", 1);
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // Fork 1: explicit tag (does NOT become the ServerTxn's sticky tag).
    uas.respond(180, "Ringing").with_to_tag("bf1").await;
    call.expect(180).await;
    // Fork 2: auto-minted tag → becomes the sticky tag `uas.dialog()` adopts.
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // bob originates the UPDATE on fork 2's early dialog toward the caller.
    let mut fork2 = uas.dialog();
    let mut update = fork2.request(InDialogMethod::Update, Some(REOFFER_HOLD)).await;
    let mut at_alice = alice.receive("UPDATE").await;
    assert!(
        String::from_utf8_lossy(&at_alice.request().body).contains("a=sendonly"),
        "fork-2 callee UPDATE relayed to alice",
    );
    at_alice.respond(200, "OK").with_sdp(OFFER).await;
    update.expect(200).await;

    // Answer on fork 2 (its sticky tag) with the SDP answer, teardown.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

// ── fake-prack (18x masking): a-leg early UPDATE ─────────────────────────────

/// Under `fake-prack`, an early a-leg UPDATE carrying an **SDP offer** must NOT
/// be answered with a bodyless local 200 (that would strand the offer, RFC 3264
/// §5). It relays to the b-leg early dialog and the callee's real answer comes
/// back — the fix behind this file's SDP-offer regression. (Bodyless refresh
/// stays local; see `fake_prack_early_bodyless_update_answered_locally`.)
#[tokio::test]
async fn fake_prack_early_update_with_offer_relays_to_bob() {
    let h = Harness::with_transit_delay("upd-fakeprack-a-offer", 1);
    let alice = h.agent("alice", "127.0.0.1:5761").await;
    let bob = h.agent("bob", "127.0.0.1:5771").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5771, RelayFirst18xStrategy::FakePrack)
        .start(&h, "b2bua", "127.0.0.1:5781")
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    // Reliable 183 → the B2BUA downgrades it to a bare 180 for alice and PRACKs bob.
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let bare180 = call.expect(180).await;
    let atag = bare180.to.tag.clone().expect("a tag on bare 180");
    bob.receive("PRACK").await.respond(200, "OK").await;

    // Alice sends an early UPDATE carrying a NEW offer.
    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&atag)
        .with_sdp(REOFFER_HOLD)
        .send()
        .await;
    // The offer must reach bob (relayed, not locally short-circuited).
    let mut at_bob = bob.receive("UPDATE").await;
    assert!(
        String::from_utf8_lossy(&at_bob.request().body).contains("a=sendonly"),
        "alice's UPDATE offer relayed to bob",
    );
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    let resp = update.expect(200).await;
    assert!(!resp.body.is_empty(), "bob's SDP answer relayed back to alice (offer answered)");
    assert!(
        get_header(&resp.headers, "content-type")
            .map(|c| c.to_ascii_lowercase().contains("application/sdp"))
            .unwrap_or(false),
        "answer carries application/sdp",
    );

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

/// Under `fake-prack`, an early a-leg **bodyless** UPDATE (session refresh, no
/// offer) is still answered 200 locally — bob is not woken, and alice's 200 has
/// no body. Pins that the offer-relay fix left the refresh short-circuit intact.
#[tokio::test]
async fn fake_prack_early_bodyless_update_answered_locally() {
    let h = Harness::with_transit_delay("upd-fakeprack-a-bodyless", 1);
    let alice = h.agent("alice", "127.0.0.1:5762").await;
    let bob = h.agent("bob", "127.0.0.1:5772").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5772, RelayFirst18xStrategy::FakePrack)
        .start(&h, "b2bua", "127.0.0.1:5782")
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let bare180 = call.expect(180).await;
    let atag = bare180.to.tag.clone().expect("a tag on bare 180");
    bob.receive("PRACK").await.respond(200, "OK").await;

    // Bodyless refresh UPDATE → answered locally, no body.
    let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&atag).send().await;
    let resp = update.expect(200).await;
    assert!(resp.body.is_empty(), "local 200 to a bodyless refresh UPDATE carries no body");

    // The call proceeds normally (bob was never woken by the refresh).
    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}
