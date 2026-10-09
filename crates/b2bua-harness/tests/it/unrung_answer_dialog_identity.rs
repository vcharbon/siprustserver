//! The dialog an answer from a branch that never rang creates is the one the
//! caller's side of the B2BUA keeps (RFC 3261 §12.1.2, §12.2.1.1).
//!
//! A forking callee rings on one branch, and a second branch answers under a
//! To-tag no provisional carried. On a transparent relay the caller was shown
//! the first branch's early dialog under its own a-facing tag; the 2xx reaches
//! her under a fresh one, so the dialog she confirms is identified by that
//! fresh tag. Every request the B2BUA sends her afterwards carries it as the
//! From-tag, and her request under the abandoned early tag names no dialog the
//! B2BUA holds (§12.2.2: `481`).
//!
//! ```text
//!   alice                     b2bua                      bob (forking)
//!     INVITE ──────────────────▶ INVITE ────────────────▶
//!     ◀── 180 (To-tag A1) ────── ◀── 180 (bobfork1)
//!     ◀── 200 (To-tag A2) ────── ◀── 200 (bobfork2, never rang)
//!     ACK ─────────────────────▶ ACK ───────────────────▶
//!     ◀── OPTIONS (From-tag A2)                         keepalive
//!     ◀── UPDATE / re-INVITE / BYE (From-tag A2) ◀───── from bob
//! ```

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{ClientInvite, Dialog, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_message::SipRequest;

/// The default keepalive interval of the scene's B2BUA.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// The answered call: alice's confirmed dialog, bob's server transaction for
/// the INVITE, and the a-facing tags of the rung and the answering branch.
struct Answered {
    dialog: Dialog,
    uas: ServerTxn,
    rung_tag: String,
    answer_tag: String,
}

/// alice calls; bob rings on `bobfork1`, then answers on `bobfork2`, which
/// never rang. Returns once alice has ACKed the 2xx and bob has the ACK.
async fn answer_on_an_unrung_branch(s: &B2buaScene) -> Answered {
    let mut call: ClientInvite =
        s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;

    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let ringing = call.expect(180).await;
    let rung_tag = ringing.to().tag().expect("the 180 carries an a-facing tag").to_string();

    uas.adopt_to_tag("bobfork2");
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let ok = call.expect(200).await;
    let answer_tag = ok.to().tag().expect("the 2xx carries an a-facing tag").to_string();
    assert_ne!(
        answer_tag, rung_tag,
        "a 2xx from a branch the caller was never shown opens a dialog of its own (§12.1.2)",
    );
    let dialog = call.ack().await;
    let ack = s.bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("bobfork2"), "the ACK rides the answering branch");
    Answered { dialog, uas, rung_tag, answer_tag }
}

/// Asserts `req`, sent toward alice, names the dialog her answer created:
/// From-tag the 2xx's To-tag, To-tag her own (§12.2.1.1).
fn names_the_answered_dialog(req: &SipRequest, a: &Answered, what: &str) {
    assert_eq!(
        req.from().tag(),
        Some(a.answer_tag.as_str()),
        "{what} toward the caller carries the To-tag of the 2xx she confirmed, not the tag \
         of the early dialog she was shown first ({})",
        a.rung_tag,
    );
    assert_eq!(req.to().tag(), Some(a.dialog.local_tag()), "{what}: To-tag is the caller's own");
}

/// The keepalive, and the UPDATE, re-INVITE and BYE relayed from the callee,
/// all reach the caller in the dialog her answer created.
#[tokio::test(start_paused = true)]
async fn requests_toward_the_caller_name_the_dialog_her_answer_created() {
    let s = B2buaScene::new("b2bua-unrung-answer-requests-toward-caller").await;
    let a = answer_on_an_unrung_branch(&s).await;

    // ── keepalive ────────────────────────────────────────────────────────────
    s.h.advance(KEEPALIVE_INTERVAL).await;
    let mut options = s.alice.receive("OPTIONS").await;
    names_the_answered_dialog(options.request(), &a, "the keepalive OPTIONS");
    options.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── bob refreshes the session with a bodiless UPDATE (RFC 3311) ──────────
    let mut bob_dialog = a.uas.dialog();
    let mut update = bob_dialog.request(InDialogMethod::Update, None).await;
    let mut alice_update = s.alice.receive("UPDATE").await;
    names_the_answered_dialog(alice_update.request(), &a, "the relayed UPDATE");
    alice_update.respond(200, "OK").await;
    update.expect(200).await;

    // ── bob re-offers ────────────────────────────────────────────────────────
    let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut alice_reinvite = s.alice.receive("INVITE").await;
    names_the_answered_dialog(alice_reinvite.request(), &a, "the relayed re-INVITE");
    alice_reinvite.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    bob_dialog.ack(None).await;
    let ack = s.alice.receive("ACK").await;
    names_the_answered_dialog(ack.request(), &a, "the relayed re-INVITE's ACK");

    // ── bob hangs up ─────────────────────────────────────────────────────────
    let mut bye = bob_dialog.bye().await;
    let mut alice_bye = s.alice.receive("BYE").await;
    names_the_answered_dialog(alice_bye.request(), &a, "the relayed BYE");
    alice_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    let _report = s.finish().await;
}

/// The caller's BYE on the early dialog she was shown first names no dialog
/// the B2BUA holds once another branch answered (§12.2.2): `481`, the call
/// stays up, and the answered dialog's own BYE ends it.
#[tokio::test(start_paused = true)]
async fn a_bye_on_the_abandoned_early_tag_draws_481() {
    let s = B2buaScene::new("b2bua-unrung-answer-abandoned-tag").await;
    let mut a = answer_on_an_unrung_branch(&s).await;

    // A request in another dialog spends nothing of this dialog's CSeq space
    // (§12.2.1.1): the counter is put back once it has left.
    let cseq_before = a.dialog.local_cseq();
    let mut stale =
        a.dialog.send_request(InDialogMethod::Bye).with_to_tag(&a.rung_tag).send().await;
    a.dialog.set_local_cseq(cseq_before);
    stale.expect(481).await;

    s.h.advance(Duration::from_millis(500)).await;
    assert!(
        s.bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a BYE naming the abandoned early dialog does not reach the callee",
    );
    assert_eq!(s.b2bua.metrics().removals_total(), 0, "the answered call stays up");

    s.hangup(&mut a.dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let _report = s.finish().await;
}

/// alice calls; bob rings on each of `rung` in turn, then answers on `winner`
/// (rung or not). Returns alice's confirmed dialog, the a-facing tag of every
/// branch she was shown, and the 2xx's To-tag, once the ACK reached bob.
async fn ring_then_answer(
    s: &B2buaScene,
    rung: &[&str],
    winner: &str,
) -> (Dialog, Vec<String>, String) {
    let mut call: ClientInvite =
        s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    let mut shown = Vec::new();
    for fork in rung {
        uas.respond(180, "Ringing").with_to_tag(fork).await;
        let ringing = call.expect(180).await;
        shown.push(ringing.to().tag().expect("the 180 carries an a-facing tag").to_string());
    }
    uas.respond(200, "OK").with_to_tag(winner).with_sdp(ANSWER_SDP).await;
    let ok = call.expect(200).await;
    let answer_tag = ok.to().tag().expect("the 2xx carries an a-facing tag").to_string();
    let dialog = call.ack().await;
    let ack = s.bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some(winner), "the ACK rides the answering branch");
    (dialog, shown, answer_tag)
}

/// Once one branch answered, every other early dialog the caller was shown
/// names no dialog the B2BUA holds (§12.2.2): a `method` request under each
/// draws `481`, nothing reaches the callee, and the answered call stays up.
/// One request per abandoned dialog: its `481` ends that dialog at the caller
/// (§12.2.1.2), so nothing follows it there.
async fn every_other_shown_tag_draws_481(
    name: &str,
    rung: &[&str],
    winner: &str,
    method: InDialogMethod,
) {
    let s = B2buaScene::new(name).await;
    let (mut dialog, shown, answer_tag) = ring_then_answer(&s, rung, winner).await;
    let others: Vec<&String> = shown.iter().filter(|t| **t != answer_tag).collect();
    assert!(!others.is_empty(), "the scenario shows the caller a branch that did not answer");
    for tag in others {
        // A request in another dialog spends nothing of this dialog's CSeq
        // space (§12.2.1.1): the counter is put back once it has left.
        let cseq_before = dialog.local_cseq();
        let mut stale = dialog.send_request(method).with_to_tag(tag).send().await;
        dialog.set_local_cseq(cseq_before);
        stale.expect(481).await;
    }
    s.h.advance(Duration::from_millis(500)).await;
    for wire in ["BYE", "UPDATE"] {
        assert!(
            s.bob.try_receive_tolerating(wire, &[]).await.is_none(),
            "a {wire} naming an abandoned early dialog does not reach the callee",
        );
    }
    assert_eq!(s.b2bua.metrics().removals_total(), 0, "the answered call stays up");
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let _report = s.finish().await;
}

const TWO_SHOWN: &[&str] = &["bobfork1", "bobfork2"];
const THREE_SHOWN: &[&str] = &["bobfork1", "bobfork2", "bobfork3"];

/// Two branches shown, a third that never rang answers.
#[tokio::test(start_paused = true)]
async fn an_unrung_answer_abandons_every_shown_branch_bye() {
    every_other_shown_tag_draws_481(
        "b2bua-unrung-two-shown-bye",
        TWO_SHOWN,
        "bobfork3",
        InDialogMethod::Bye,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_unrung_answer_abandons_every_shown_branch_update() {
    every_other_shown_tag_draws_481(
        "b2bua-unrung-two-shown-update",
        TWO_SHOWN,
        "bobfork3",
        InDialogMethod::Update,
    )
    .await;
}

/// Three branches shown, the second answers: the first and the third are
/// abandoned alike.
#[tokio::test(start_paused = true)]
async fn a_rung_later_answer_abandons_every_other_shown_branch_bye() {
    every_other_shown_tag_draws_481(
        "b2bua-rung-later-bye",
        THREE_SHOWN,
        "bobfork2",
        InDialogMethod::Bye,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_rung_later_answer_abandons_every_other_shown_branch_update() {
    every_other_shown_tag_draws_481(
        "b2bua-rung-later-update",
        THREE_SHOWN,
        "bobfork2",
        InDialogMethod::Update,
    )
    .await;
}

/// Two branches shown, the first answers: the second is abandoned.
#[tokio::test(start_paused = true)]
async fn a_rung_first_answer_abandons_the_other_shown_branch_bye() {
    every_other_shown_tag_draws_481(
        "b2bua-rung-first-bye",
        TWO_SHOWN,
        "bobfork1",
        InDialogMethod::Bye,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_rung_first_answer_abandons_the_other_shown_branch_update() {
    every_other_shown_tag_draws_481(
        "b2bua-rung-first-update",
        TWO_SHOWN,
        "bobfork1",
        InDialogMethod::Update,
    )
    .await;
}
