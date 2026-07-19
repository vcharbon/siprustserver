//! UAS multi-early-dialog send surface: on ONE INVITE server transaction a UAS
//! creates and drives arbitrary early dialogs (each an early-id → distinct
//! To-tag), a simulated downstream fork. Exactly one wins the final 2xx; the
//! losers settle by the protocol automatics. SUT-less, through the
//! recording-wrapped simulated network.

use scenario_harness::{EmitOpts, Harness, MessageTemplate, TemplateHeader};
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// The workflow acceptance: two early dialogs — e1 (180) and e2 (reliable
/// 183+SDP) — on one server transaction; e2 wins with the 200; e1 settles (no
/// final). The UAC observes the distinct early dialogs by To-tag and ACKs the
/// winner. Audit green, BYE-terminated.
#[tokio::test]
async fn two_early_dialogs_winner_answers() {
    let h = Harness::new("early-two-forks").describe(
        "one INVITE server txn drives e1 (180) + e2 (reliable 183+SDP); e2 wins \
         the 200, e1 settles; the UAC ACKs the winner",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).with_header("Supported", "100rel").send().await;
    let mut uas = bob.receive("INVITE").await;

    // Two early dialogs on the ONE server transaction.
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.respond_early("e2", 183, "Session Progress").reliable(1).with_sdp(ANSWER).send().await;

    // The UAC sees two DISTINCT early dialogs (per-To-tag view of provisionals).
    let p1 = call.expect(180).await;
    let t1 = p1.to.tag.clone().expect("e1 To-tag");
    let p2 = call.expect(183).await;
    let t2 = p2.to.tag.clone().expect("e2 To-tag");
    assert_ne!(t1, t2, "distinct early dialogs carry distinct To-tags");

    // PRACK e2's reliable 183 (RFC 3262 MUST-014) — routed to e2's To-tag.
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&t2)
        .with_rack("1 1 INVITE")
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    // e2 wins; e1 settles by the automatics (no final on e1).
    uas.win("e2");
    uas.respond(200, "OK").send().await; // answer already sent in the reliable 183
    let ok = call.expect(200).await;
    assert_eq!(ok.to.tag.as_deref(), Some(t2.as_str()), "the 2xx is under the winner e2");

    let mut dialog = call.ack().await; // ACK addresses only the winner
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// Ordering independence: a three-way fork where the winner is NOT the
/// last-created early dialog (e2 wins, e3 created after it).
#[tokio::test]
async fn three_way_fork_winner_not_last() {
    let h = Harness::new("early-three-forks").describe(
        "three early dialogs e1/e2/e3 (180 each); e2 wins the 200 though e3 was \
         created last — the winner is any early dialog, not the last",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.respond_early("e2", 180, "Ringing").send().await;
    uas.respond_early("e3", 180, "Ringing").send().await;

    let t1 = call.expect(180).await.to.tag.clone().expect("e1");
    let t2 = call.expect(180).await.to.tag.clone().expect("e2");
    let t3 = call.expect(180).await.to.tag.clone().expect("e3");
    assert!(t1 != t2 && t2 != t3 && t1 != t3, "three distinct early dialogs");

    // The winner is e2 — created BEFORE e3.
    uas.win("e2");
    uas.respond(200, "OK").with_sdp(ANSWER).send().await; // offer in INVITE, answer here
    let ok = call.expect(200).await;
    assert_eq!(ok.to.tag.as_deref(), Some(t2.as_str()), "e2 wins, not the last-created e3");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// The §6.1b generalization ForkingRing lacks: an in-early-dialog UPDATE
/// (UAC→UAS) in e2's early dialog BEFORE the final, correctly routed to e2's
/// To-tag. e1 is a loser (180). RFC 3311 §5.1: the early UPDATE renegotiates
/// after the reliable 183 completed the initial offer/answer.
#[tokio::test]
async fn early_dialog_update_before_final() {
    let h = Harness::new("early-update").describe(
        "e2 (reliable 183+SDP) takes an in-early-dialog UPDATE from the UAC \
         before the final; e1 settles; e2 wins",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).with_header("Supported", "100rel").send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.respond_early("e2", 183, "Session Progress").reliable(1).with_sdp(ANSWER).send().await;

    let _t1 = call.expect(180).await.to.tag.clone().expect("e1");
    let t2 = call.expect(183).await.to.tag.clone().expect("e2");

    // PRACK e2 (completes the initial offer/answer on the early dialog).
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&t2)
        .with_rack("1 1 INVITE")
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    // In-early-dialog UPDATE (RFC 3311 §5.1) in e2, before the final.
    let mut upd = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&t2)
        .with_sdp(OFFER)
        .send()
        .await;
    let mut ubob = bob.receive("UPDATE").await;
    assert_eq!(
        ubob.request().to.tag.as_deref(),
        Some(t2.as_str()),
        "the UPDATE is routed to e2's early dialog",
    );
    ubob.respond(200, "OK").with_sdp(ANSWER).await; // 200 echoes e2's To-tag
    upd.expect(200).await;

    // e2 wins.
    uas.win("e2");
    uas.respond(200, "OK").send().await;
    call.expect(200).await;

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

// --- Silent-misuse guards -------------------------------------------------
// These misuses are all wire-legal, so the audit cannot catch them; the send
// surface must reject them at the point of misuse.

/// Declaring a winner that never opened an early dialog (no provisional emitted)
/// would silently mint a fresh tag and answer a fork the UAC never saw.
#[tokio::test]
#[should_panic(expected = "no provisional was emitted")]
async fn win_without_provisional_panics() {
    let h = Harness::new("win-no-provisional");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let _call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.win("ghost"); // never emitted a provisional
}

/// A transaction has exactly one winner; a second, different `win` would
/// last-win silently and answer the wrong early dialog.
#[tokio::test]
#[should_panic(expected = "already won this transaction")]
async fn second_different_winner_panics() {
    let h = Harness::new("two-winners");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let _call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.respond_early("e2", 180, "Ringing").send().await;
    uas.win("e1");
    uas.win("e2"); // a second, different winner
}

/// Sending the final 2xx with early dialogs open and no winner declared would
/// answer on a phantom fresh mint — a fork the UAC never saw.
#[tokio::test]
#[should_panic(expected = "declare the winner with win(id)")]
async fn forked_final_without_winner_panics() {
    let h = Harness::new("final-no-winner");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let _call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.respond_early("e2", 180, "Ringing").send().await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await; // no win() first
}

/// `respond_early` is for provisionals; a final under an early tag never adopts
/// the sticky dialog tag and detonates cryptically at the later BYE.
#[tokio::test]
#[should_panic(expected = "respond_early is for provisionals")]
async fn respond_early_final_status_panics() {
    let h = Harness::new("early-final");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let _call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 200, "OK").with_sdp(ANSWER).send().await;
}

/// A new 1xx after the transaction's final is illegal (RFC 3261 §17.2.1) and
/// audit-visible only via the new rule; the surface rejects it up front.
#[tokio::test]
#[should_panic(expected = "already finalized with 200")]
async fn provisional_after_final_panics() {
    let h = Harness::new("late-provisional");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let _call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.win("e1");
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    uas.respond(181, "Queued").send().await; // a new 1xx after the final
}

/// A TEMPLATE-driven provisional targeting a specific early dialog: e2's 180 is
/// built from a template whose frozen headers are byte-preserved (U3 semantics)
/// and carried under e2's minted To-tag.
#[tokio::test]
async fn template_provisional_targets_early_dialog() {
    let h = Harness::new("early-template").describe(
        "respond_template_early builds e2's 180 from a template; frozen headers \
         byte-preserved, To-tag = the early dialog's",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;

    let tmpl = MessageTemplate::response(
        180,
        "Ringing",
        vec![
            TemplateHeader::frozen("Subject", "fork-e2"),
            TemplateHeader::frozen("X-Trace", "kept"),
        ],
        Vec::new(),
    );
    uas.respond_early("e1", 180, "Ringing").send().await;
    uas.respond_template_early("e2", &tmpl, EmitOpts::default()).send().await;

    let p1 = call.expect(180).await;
    let t1 = p1.to.tag.clone().expect("e1");
    let p2 = call.expect(180).await;
    let t2 = p2.to.tag.clone().expect("e2");
    assert_ne!(t1, t2, "distinct early dialogs");
    // e2's template provisional: frozen headers byte-preserved, own To-tag.
    assert_eq!(get_header(&p2.headers, "subject"), Some("fork-e2"), "frozen Subject preserved");
    assert_eq!(get_header(&p2.headers, "x-trace"), Some("kept"), "frozen X-Trace preserved");

    uas.win("e2");
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to.tag.as_deref(), Some(t2.as_str()), "winner is e2");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}
