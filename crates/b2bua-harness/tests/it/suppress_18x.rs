//! suppress-18x — `relayFirst18xTo180` strategy `drop-sdp` (wire `true`). Port of
//! `tests/scenarios/suppress-18x.ts`.
//!
//! The B2BUA rewrites the first 18x from any b-leg into a bare 180 (no SDP / no
//! 100rel) and suppresses later 18x. A reliable 1xx is PRACKed by the B2BUA
//! itself (alice never sees it). The 200 OK answers under the tag of the caller
//! dialog its callee dialog was shown as: the dialog behind the bare 180 keeps
//! that 180's To-tag; a callee dialog the caller was never shown (a suppressed
//! fork, a rerouted leg) opens a caller dialog of its own under a fresh To-tag
//! (RFC 3261 §12.1.2, §13.2.2.4).
//!
//! Failover cases (`failoverNoAnswer`, `failoverReject`) ride the `/call/failure`
//! b-leg failover path: bob2 rang behind the mask, so its 200 opens the second
//! caller dialog; the non-2xx finals stay on the owned 180's tag (§17.2.1).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to_with_18x;
use b2bua::decision::{CallFailureResponse, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use call::CdrEventType;
use scenario_harness::Harness;
use sip_message::error::SipParseError;
use sip_message::generators::InDialogMethod;
use sip_message::header::kind::TokenKind;
use sip_message::header::{RAck, RSeq, Require, Supported, TokenListHeader};
use sip_message::Method;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Whether an option-tag list header states `token`. An absent header states
/// nothing; one no reader accepts is a defect the test must not hide.
fn has_token<K: TokenKind>(
    header: Option<Result<TokenListHeader<K>, SipParseError>>,
    token: &str,
) -> bool {
    header.is_some_and(|h| h.expect("readable option-tag list").contains(token))
}

#[tokio::test]
async fn basic() {
    let h = Harness::with_transit_delay("suppress-18x-basic", 0);
    let alice = h.agent("alice", "127.0.0.1:5601").await;
    let bob = h.agent("bob", "127.0.0.1:5611").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5611, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5621")
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
        !has_token(uas.request().header::<Supported>(), "100rel"),
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
    assert!(p180.body().is_empty(), "bare 180 has no body");
    assert!(!has_token(p180.header::<Require>(), "100rel"), "no Require:100rel on bare 180",);
    assert!(p180.header::<RSeq>().is_none(), "no RSeq on bare 180");
    let first_to_tag = p180.to().tag().expect("180 has a To-tag").to_string();

    // The B2BUA PRACKs bob (alice never saw the reliable provisional).
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(
        prack.request().header::<RAck>().expect("a RAck").expect("readable RAck"),
        RAck::new(1, 1, Method::Invite),
        "RAck = rseq 1, INVITE cseq 1",
    );
    prack.respond(200, "OK").await;

    // Bob sends another 180 — suppressed (alice receives nothing more here).
    uas.respond(180, "Ringing").await;

    // Bob answers with SDP; alice's 200 reuses the first 180's To-tag.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(first_to_tag.as_str()), "200 To-tag == 180 To-tag");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// One callee, two early dialogs on the one INVITE (RFC 3261 §12.1 forking):
/// fork 1 rings and is shown as the bare 180, fork 2 rings behind the mask and
/// answers. The caller never saw fork 2, so its 200 opens a caller dialog of
/// its own (§12.1.2) under a fresh To-tag, and that dialog carries her ACK,
/// her BYE and the B2BUA's own in-dialog request toward her.
#[tokio::test]
async fn a_fork_the_caller_never_saw_answers_under_its_own_dialog() {
    let h = Harness::with_transit_delay("suppress-18x-unshown-fork-answers", 0);
    let alice = h.agent("alice", "127.0.0.1:5651").await;
    let bob = h.agent("bob", "127.0.0.1:5661").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5661, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5671")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // Fork 1 rings → the bare 180 the caller sees, under the owned tag.
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let shown_tag = owned_180_tag(&mut call).await;

    // Fork 2 rings behind the mask (suppressed), then answers.
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    uas.adopt_to_tag("bobfork2");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    let answered_tag = ok.to().tag().expect("200 has a To-tag").to_string();
    assert_ne!(answered_tag, shown_tag, "the unshown fork's 200 opens a second caller dialog");
    assert_eq!(ok.body(), ANSWER.as_bytes(), "the 200 carries the answering fork's SDP");

    // The caller's ACK confirms the dialog the 200 opened and reaches fork 2.
    let dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("bobfork2"), "the ACK rides fork 2's dialog");

    // The dialog she rang on is abandoned: a request she sends under the 180's
    // tag matches no dialog the B2BUA holds and draws 481 (RFC 3261 §12.2.2),
    // never the answered session.
    let mut stale = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&shown_tag)
        .with_sdp(OFFER)
        .send()
        .await;
    stale.expect(481).await;

    // Bob's BYE reaches the caller inside the confirmed dialog: the B2BUA's
    // From-tag toward her is the tag the 200 carried, not the 180's.
    let mut bob_dialog = uas.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    let mut bye_at_alice = alice.receive("BYE").await;
    assert_eq!(
        bye_at_alice.request().from().tag(),
        Some(answered_tag.as_str()),
        "the BYE toward the caller names the dialog the 200 opened",
    );
    bye_at_alice.respond(200, "OK").await;
    bob_bye.expect(200).await;
    drop(dialog);

    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Failover on reject: bob1 sends 180 then 503; the B2BUA fails over via
/// `/call/failure` to bob2 (new R-URI), which answers. Alice never sees the 503;
/// her 200 OK opens a second caller dialog (bob2 rang behind the mask) and the
/// dialog it confirms carries her ACK and BYE.
/// (TS `suppress18xFailoverReject`.)
#[tokio::test]
async fn failover_reject() {
    let h = Harness::with_transit_delay("suppress-18x-failover-reject", 0);
    let alice = h.agent("alice", "127.0.0.1:5603").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5613").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5614").await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        "127.0.0.1",
        5613,
        5614,
        "sip:+1234@127.0.0.1:5614",
        RelayFirst18xStrategy::DropSdp,
    )
    .start(&h, "b2bua", "127.0.0.1:5623")
    .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;

    // Bob1 receives the INVITE, rings, then rejects.
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;

    let p180 = call.expect(180).await;
    let first_to_tag = p180.to().tag().expect("180 has a To-tag").to_string();

    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await; // the b2bua completes bob1's reject txn (§17.1.1.3)

    // Failover to bob2 (new R-URI per the on_failure decision).
    let mut uas2 = bob2.receive("INVITE").await;
    assert_eq!(
        uas2.request().request_uri().text(),
        "sip:+1234@127.0.0.1:5614",
        "bob2 R-URI is the failover new_ruri"
    );

    // Bob2's 18x are suppressed (alice already saw the bare 180 from bob1).
    uas2.respond(180, "Ringing").await;
    uas2.respond(180, "Ringing").await;

    // Bob2 answers; alice's 200 opens a caller dialog of its own — bob1's 180
    // pinned the first tag, bob2 was never shown.
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    let answered_tag = ok.to().tag().expect("200 has a To-tag").to_string();
    assert_ne!(answered_tag, first_to_tag, "the unshown bob2's 200 opens a second caller dialog");

    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// Failover on no-answer: delayed-offer INVITE; bob1 rings then times out; the
/// B2BUA CANCELs bob1 and fails over via `/call/failure` to bob2 (new R-URI),
/// which answers with the offer. Alice's 200 opens a second caller dialog (bob2
/// rang behind the mask), and her ACK answers the offer inside it.
/// (TS `suppress18xFailoverNoAnswer`.)
#[tokio::test(start_paused = true)]
async fn failover_no_answer() {
    let h = Harness::with_transit_delay("suppress-18x-failover-no-answer", 0);
    let alice = h.agent("alice", "127.0.0.1:5605").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5615").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5616").await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        "127.0.0.1",
        5615,
        5616,
        "sip:+1234@127.0.0.1:5616",
        RelayFirst18xStrategy::DropSdp,
    )
    .start(&h, "b2bua", "127.0.0.1:5625")
    .await;

    // Delayed offer: no SDP in the INVITE — bob2's 200 OK carries the offer.
    let mut call = alice.invite(&bob1).through(b2bua.addr).send().await;

    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;

    let p180 = call.expect(180).await;
    let first_to_tag = p180.to().tag().expect("180 has a To-tag").to_string();

    // No-answer timeout (30 s) → CANCEL bob1 + failover to bob2. Advance just
    // past the deadline (not a full second) so the failover INVITE to bob2 is
    // answered before its Timer A retransmit (~500 ms) leaks under the paused
    // clock — the deterministic equivalent of the TS `bob2.allowExtra("INVITE")`.
    h.advance(Duration::from_secs(30) + Duration::from_millis(100)).await;

    // Bob1 gets the CANCEL (tied to its still-open INVITE txn), 200s it, then
    // 487s the INVITE; the B2BUA auto-ACKs the 487.
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    uas1.respond(487, "Request Terminated").await;
    bob1.receive("ACK").await; // the b2bua completes bob1's 487 txn (§17.1.1.3)

    // Bob2 receives the failover INVITE with the configured new R-URI.
    let mut uas2 = bob2.receive("INVITE").await;
    assert_eq!(
        uas2.request().request_uri().text(),
        "sip:+1234@127.0.0.1:5616",
        "bob2 R-URI is the failover new_ruri"
    );

    // Bob2's 180 is suppressed; its 200 OK carries the (delayed) SDP offer.
    uas2.respond(180, "Ringing").await;
    uas2.respond(200, "OK").with_sdp(OFFER).await;

    let ok = call.expect(200).await;
    assert_ne!(
        ok.to().tag().expect("200 has a To-tag"),
        first_to_tag,
        "the unshown bob2's 200 opens a second caller dialog",
    );

    // Alice answers the delayed offer in the ACK (RFC 3264 §4).
    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let ack = bob2.receive("ACK").await;
    assert!(!ack.request().body().is_empty(), "ACK carries the SDP answer");

    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

// ── relay18x.messages policy (GAP-P7-2) ─────────────────────────────────────
// The Routing API `Relay18x.messages` field picks WHICH 18x are relayed (each
// relayed one still downgraded to a bare 180 under the SAME stored To-tag):
// FIRST (default, covered by the tests above), ALL, ONE_PER_VALUE (one per
// distinct *upstream* status value). `expect` is strict (anything but the
// expected status panics), so the `expect(200)` after the last expected 180
// doubles as the "nothing extra was relayed" suppression assert.

/// `messages = ALL`, a forked callee: every fork's 18x is relayed as a bare 180
/// under the first 180's To-tag, so every fork was SHOWN — the fork that
/// answers, first or not, answers under that one tag.
#[tokio::test]
async fn messages_all_a_relayed_fork_answers_under_the_180_s_tag() {
    let h = Harness::with_transit_delay("suppress-18x-messages-all-fork", 0);
    let alice = h.agent("alice", "127.0.0.1:5653").await;
    let bob = h.agent("bob", "127.0.0.1:5663").await;
    let b2bua = B2buaSut::route_all_to_with_18x_messages(
        "127.0.0.1",
        5663,
        RelayFirst18xStrategy::DropSdp,
        call::features::Relay18xMessages::All,
    )
    .start(&h, "b2bua", "127.0.0.1:5673")
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag = owned_180_tag(&mut call).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let p2 = call.expect(180).await;
    assert_eq!(p2.to().tag(), Some(a_tag.as_str()), "fork 2's 180 is shown under the same tag");

    uas.adopt_to_tag("bobfork2");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag.as_str()), "a shown fork answers under the 180's tag");

    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("bobfork2"), "the ACK rides fork 2's dialog");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// `messages = ALL`: every 18x bob sends is relayed, each downgraded to a bare
/// 180 under the first 180's To-tag.
#[tokio::test]
async fn messages_all_relays_every_18x_downgraded() {
    let h = Harness::with_transit_delay("suppress-18x-messages-all", 0);
    let alice = h.agent("alice", "127.0.0.1:5607").await;
    let bob = h.agent("bob", "127.0.0.1:5617").await;
    let b2bua = B2buaSut::route_all_to_with_18x_messages(
        "127.0.0.1",
        5617,
        RelayFirst18xStrategy::DropSdp,
        call::features::Relay18xMessages::All,
    )
    .start(&h, "b2bua", "127.0.0.1:5627")
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // 180 → bare 180 (mints the stored a-facing tag).
    uas.respond(180, "Ringing").await;
    let p1 = call.expect(180).await;
    assert!(p1.body().is_empty(), "first relayed 18x is a bare 180");
    let a_tag = p1.to().tag().expect("180 has a To-tag").to_string();

    // 183 with SDP → relayed again, STILL downgraded: bare 180, same To-tag.
    uas.respond(183, "Session Progress").with_sdp(ANSWER).await;
    let p2 = call.expect(180).await;
    assert!(p2.body().is_empty(), "later relayed 18x is downgraded (no SDP)");
    assert_eq!(p2.to().tag(), Some(a_tag.as_str()), "same stored To-tag (one early dialog)");

    // A third 18x → also relayed (ALL).
    uas.respond(180, "Ringing").await;
    let p3 = call.expect(180).await;
    assert_eq!(p3.to().tag(), Some(a_tag.as_str()));

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag.as_str()), "200 To-tag == first 180 To-tag");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// `messages = ONE_PER_VALUE`: one relay per distinct *upstream* status value —
/// bob's first 183 and first 180 are relayed (both as bare 180s under the same
/// To-tag), the repeated 183/180 are suppressed.
#[tokio::test]
async fn messages_one_per_value_dedupes_on_upstream_status() {
    let h = Harness::with_transit_delay("suppress-18x-messages-one-per-value", 0);
    let alice = h.agent("alice", "127.0.0.1:5608").await;
    let bob = h.agent("bob", "127.0.0.1:5618").await;
    let b2bua = B2buaSut::route_all_to_with_18x_messages(
        "127.0.0.1",
        5618,
        RelayFirst18xStrategy::DropSdp,
        call::features::Relay18xMessages::OnePerValue,
    )
    .start(&h, "b2bua", "127.0.0.1:5628")
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // First 183 → relayed as the bare 180 (value 183 now spent).
    uas.respond(183, "Session Progress").with_sdp(ANSWER).await;
    let p1 = call.expect(180).await;
    assert!(p1.body().is_empty(), "bare 180");
    let a_tag = p1.to().tag().expect("180 has a To-tag").to_string();

    // Second 183 → suppressed (same upstream value).
    uas.respond(183, "Session Progress").with_sdp(ANSWER).await;

    // First 180 → a NEW upstream value → relayed (bare 180, same To-tag).
    uas.respond(180, "Ringing").await;
    let p2 = call.expect(180).await;
    assert_eq!(p2.to().tag(), Some(a_tag.as_str()), "same stored To-tag");

    // Second 180 → suppressed. If either suppressed 18x had been relayed, the
    // strict expect(200) below would see it first and panic.
    uas.respond(180, "Ringing").await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag.as_str()), "200 To-tag == first 180 To-tag");

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
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5612).start(&h, "b2bua", "127.0.0.1:5622").await;

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

// ── the owned tag on a caller-facing FINAL ──────────────────────────────────
// Under this mask the B2BUA owns the caller-facing provisional: it shows the
// caller ONE bare 180 under ONE tag however many b-forks ring and however many
// b-legs the failover walks. Every response to that INVITE therefore carries
// that one tag (RFC 3261 §8.2.6.2) — the relayed 18x and the 200 (pinned above),
// the relayed non-2xx final, and the transaction layer's own 487 on the caller's
// CANCEL, which resolves the To-tag pinned by the first response above 100
// (§17.2.1) and so independently names the same value.

/// The owned 180's To-tag, and the b2bua the caller talks to.
async fn owned_180_tag(call: &mut scenario_harness::agent::ClientInvite) -> String {
    let p180 = call.expect(180).await;
    assert!(p180.body().is_empty(), "bare 180 has no body");
    p180.to().tag().expect("180 has a To-tag").to_string()
}

/// The recorded-trace audit charges no To-tag flip on this call. `tag-consistency`
/// (§17.2.1 / §12.1.1) is force-advisory in the live bind — a forking B2BUA
/// answering 2xx off a LATER early dialog is §13.2.2.4-legitimate and, per server
/// transaction, indistinguishable from a flip. Under this mask the caller holds
/// exactly ONE early dialog, so nothing here is that case and these rungs gate on
/// the rule themselves.
fn assert_no_tag_flip(report: &scenario_harness::RunReport) {
    let flips: Vec<&str> = report
        .rfc_findings()
        .iter()
        .filter(|f| f.rule == "tag-consistency")
        .map(|f| f.detail.as_str())
        .collect();
    assert!(flips.is_empty(), "the audit charges a To-tag flip: {flips:?}");
}

/// The rejected call left one CDR carrying a reject, and the B2BUA holds nothing.
async fn assert_rejected_and_reaped(b2bua: &B2buaSut) {
    settle_until(|| !b2bua.cdr_records().is_empty() && b2bua.active_calls() == 0).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the rejected call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Reject), "reject event: {kinds:?}");
    b2bua.assert_fully_reaped();
}

/// One callee, one 180, one 486 — no forking, no failover. The caller's early
/// dialog and the final that ends it carry the same tag.
#[tokio::test]
async fn an_unforked_rejection_rides_the_owned_180_s_tag() {
    let h = Harness::with_transit_delay("suppress-18x-unforked-rejection-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:5604").await;
    let bob = h.agent("bob", "127.0.0.1:5619").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5619, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5624")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let owned = owned_180_tag(&mut call).await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await; // the b2bua completes bob's reject txn (§17.1.1.3)

    let rejected = call.expect(486).await;
    assert_eq!(
        rejected.to().tag(),
        Some(owned.as_str()),
        "the 486 answers under the tag the owned 180 put the caller in",
    );

    assert_rejected_and_reaped(&b2bua).await;
    alice.drain().await;
    bob.drain().await;
    assert_no_tag_flip(&h.finish().await);
}

/// A NON-FIRST b-fork rejects. The mask gives the caller exactly ONE early
/// dialog however many forks ring, so the 503 rides that one tag — while the
/// B2BUA's own b-facing ACK stays in fork 2, where the 503 was generated.
#[tokio::test]
async fn a_forked_rejection_rides_the_owned_180_s_tag() {
    let h = Harness::with_transit_delay("suppress-18x-forked-rejection-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:5606").await;
    let bob = h.agent("bob", "127.0.0.1:5620").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5620, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5626")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let owned = owned_180_tag(&mut call).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await; // suppressed

    uas.respond(503, "Service Unavailable").with_to_tag("bobfork2").await;
    let b_ack = bob.receive("ACK").await;
    assert_eq!(
        b_ack.request().to().tag(),
        Some("bobfork2"),
        "the b-facing ACK acknowledges the response where it was generated",
    );

    let rejected = call.expect(503).await;
    assert_eq!(
        rejected.to().tag(),
        Some(owned.as_str()),
        "one masked early dialog → the 503 rides the owned tag, not the rejecting fork's",
    );

    assert_rejected_and_reaped(&b2bua).await;
    alice.drain().await;
    bob.drain().await;
    assert_no_tag_flip(&h.finish().await);
}

/// The rejection arrives on the SECOND b-leg, after a `/call/failure` leg swap.
/// The `relay_first_18x` slice is deliberately not cleared on failover, so the
/// owned tag survives it on a final; the 200 of that leg is what opens a
/// dialog of its own (`failover_reject`).
#[tokio::test]
async fn a_rejection_after_failover_rides_the_owned_180_s_tag() {
    let h = Harness::with_transit_delay("suppress-18x-failover-rejection-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:5609").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5629").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5630").await;
    // One-deep plan: only the FIRST b-leg's failure reroutes, so attempt 2's
    // rejection is the last word and relays to the caller.
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to_with_18x("127.0.0.1", 5629, RelayFirst18xStrategy::DropSdp);
                r.callback_context = Some("suppress-18x-failover-rejection".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|req| {
                if req.failure.failed_leg_id.as_deref() == Some("b-1") {
                    let mut r =
                        route_to_with_18x("127.0.0.1", 5630, RelayFirst18xStrategy::DropSdp);
                    r.new_ruri = Some("sip:+1234@127.0.0.1:5630".into());
                    r.callback_context = Some("suppress-18x-failover-rejection".into());
                    CallFailureResponse::Route(r)
                } else {
                    CallFailureResponse::Relay { label: None }
                }
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5631").await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    let owned = owned_180_tag(&mut call).await;

    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;

    // Attempt 2 rings on two forks (both suppressed), then a non-first fork
    // rejects — the last attempt, so the failure relays to the caller.
    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(180, "Ringing").with_to_tag("bob2fork1").await;
    uas2.respond(180, "Ringing").with_to_tag("bob2fork2").await;
    uas2.respond(486, "Busy Here").with_to_tag("bob2fork2").await;
    bob2.receive("ACK").await;

    let rejected = call.expect(486).await;
    assert_eq!(
        rejected.to().tag(),
        Some(owned.as_str()),
        "the owned tag survives the leg swap on a final, as it does on a 200",
    );

    assert_rejected_and_reaped(&b2bua).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    assert_no_tag_flip(&h.finish().await);
}

/// The caller CANCELs its own ringing call. The 487 is generated by the
/// transaction layer off the To-tag it pinned on the first response above 100
/// (§17.2.1) — the owned 180's. It must agree with the relayed finals above:
/// one INVITE transaction never ends twice on two tags.
#[tokio::test]
async fn the_cancelled_call_s_487_rides_the_owned_180_s_tag() {
    let h = Harness::with_transit_delay("suppress-18x-cancel-487-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:5610").await;
    let bob = h.agent("bob", "127.0.0.1:5632").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5632, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5633")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    // Bob pins ONE tag across the INVITE transaction — its 180, its 200 to the
    // CANCEL (which shares the INVITE's branch, §9.1) and its 487 — so the only
    // tag identity this rung reads is the SUT's own.
    uas.respond(180, "Ringing").with_to_tag("bob1").await;
    let owned = owned_180_tag(&mut call).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    let terminated = call.expect(487).await;
    assert_eq!(
        terminated.to().tag(),
        Some(owned.as_str()),
        "the autonomous 487 answers under the tag the owned 180 pinned",
    );

    // The b-leg is CANCELled and completes its own 487 transaction (§17.1.1.3).
    bob.receive("CANCEL").await.respond(200, "OK").with_to_tag("bob1").await;
    uas.respond(487, "Request Terminated").with_to_tag("bob1").await;
    bob.receive("ACK").await;

    settle_until(|| !b2bua.cdr_records().is_empty() && b2bua.active_calls() == 0).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the cancelled call");
    b2bua.assert_fully_reaped();
    alice.drain().await;
    bob.drain().await;
    assert_no_tag_flip(&h.finish().await);
}

/// A stray PRACK is answered HERE, not handed to the callee — `drop-sdp` does
/// not change who owes the answer.
///
/// The `RSeq` space a PRACK names is the one this stack mints toward the caller
/// (RFC 3262 §4, errata 4603/4604), and under this strategy it mints none: the
/// first 18x leaves as a bare 180, so alice was shown no reliable provisional
/// and no `RAck` she can send matches anything. That makes every PRACK on this
/// face unmatched, and §4 owes each one a `481` from the face that would have
/// sent the provisional.
///
/// Relaying it puts the decision in bob's independent sequence space (§3,
/// errata 4600), where a coincidental match has him acknowledge a provisional
/// alice was never shown — and bob was never told to expect PRACK at all here,
/// `100rel` having been stripped from his `Supported`.
#[tokio::test]
async fn a_stray_prack_is_answered_here_and_never_reaches_the_callee() {
    let h = Harness::with_transit_delay("suppress-18x-stray-prack", 0);
    let alice = h.agent("alice", "127.0.0.1:5640").await;
    let bob = h.agent("bob", "127.0.0.1:5641").await;
    let b2bua = B2buaSut::route_all_to_with_18x("127.0.0.1", 5641, RelayFirst18xStrategy::DropSdp)
        .start(&h, "b2bua", "127.0.0.1:5642")
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;

    // Bob honours the stripped `Supported` and rings unreliably, so no PRACK
    // belongs anywhere on this call: the one below is the only one on the wire.
    let mut uas = bob.receive("INVITE").await;
    assert!(
        !has_token(uas.request().header::<Supported>(), "100rel"),
        "100rel stripped from bob's Supported",
    );
    uas.respond(180, "Ringing").await;

    let p180 = call.expect(180).await;
    assert!(p180.header::<RSeq>().is_none(), "the bare 180 names no sequence");

    // Alice PRACKs regardless — the misbehaving caller this fixture is for.
    let (mut stray, sent) = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("1 {} INVITE", p180.cseq().seq()))
        .try_send_with_request()
        .await
        .expect("the PRACK goes out");
    assert_eq!(
        sent.to().tag(),
        p180.to().tag(),
        "the PRACK rides the dialog the 180 opened (RFC 3262 §5)",
    );
    stray.expect(481).await;

    // The call still completes: a refused PRACK ends nothing.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // Nothing PRACK-shaped ever crossed onto the b leg — the whole point, and a
    // claim about the WHOLE run rather than about what bob happened to read next.
    let bob_addr: std::net::SocketAddr = "127.0.0.1:5641".parse().unwrap();
    let leaked = h
        .wire_entries()
        .into_iter()
        .filter(|e| e.to == bob_addr && e.raw.starts_with(b"PRACK "))
        .count();
    assert_eq!(leaked, 0, "the callee saw {leaked} PRACK(s) on a leg that negotiated none");

    let _ = h.finish().await;
}

/// The leg a `/call/failure` reroute mints is solicited the same reliability
/// as the first: `drop-sdp` keeps alice unreliable and relays no PRACK, so
/// `100rel` she offered rides neither bob1's INVITE nor bob2's (RFC 3262 §3 —
/// an offer this stack cannot honour is not made), whichever mint assembled
/// the message.
#[tokio::test]
async fn the_rerouted_leg_is_solicited_the_same_reliability_as_the_first() {
    let h = Harness::with_transit_delay("suppress-18x-reroute-same-offer", 0);
    let alice = h.agent("alice", "127.0.0.1:5643").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5644").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5645").await;
    let b2bua = B2buaSut::route_all_to_with_18x_failover(
        "127.0.0.1",
        5644,
        5645,
        "sip:+1234@127.0.0.1:5645",
        RelayFirst18xStrategy::DropSdp,
    )
    .start(&h, "b2bua", "127.0.0.1:5646")
    .await;

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, timer")
        .through(b2bua.addr)
        .send()
        .await;

    let mut uas1 = bob1.receive("INVITE").await;
    assert!(
        !has_token(uas1.request().header::<Supported>(), "100rel"),
        "100rel withheld from bob1's Supported",
    );
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;

    // The rerouted leg: the same withhold, the rest of alice's set relayed.
    let mut uas2 = bob2.receive("INVITE").await;
    let supported = uas2.request().header::<Supported>().expect("a Supported line").unwrap();
    assert!(!supported.contains("100rel"), "100rel withheld from bob2's Supported");
    assert!(supported.contains("timer"), "the rest of alice's set rides bob2's INVITE");

    uas2.respond(180, "Ringing").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}

/// A declared advertisement toward the callee stands under `drop-sdp`, narrowed
/// by the strategy's withhold rather than replaced by alice's relayed set: bob
/// is told `timer` — the declaration — and not `100rel`, whichever of the two
/// stated it.
#[tokio::test]
async fn a_declared_advertisement_is_narrowed_by_the_strategy_not_discarded() {
    let h = Harness::with_transit_delay("suppress-18x-declared-advert", 0);
    let alice = h.agent("alice", "127.0.0.1:5647").await;
    let bob = h.agent("bob", "127.0.0.1:5648").await;
    let b2bua = B2buaSut::builder(Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to_with_18x("127.0.0.1", 5648, RelayFirst18xStrategy::DropSdp);
                r.features.advertise_capabilities =
                    Some(call::features::AdvertiseCapabilitiesFeature {
                        toward_originator: None,
                        toward_originated: Some(call::features::AdvertisedCapabilities {
                            allow: None,
                            supported: Some(vec!["timer".to_string(), "100rel".to_string()]),
                        }),
                    });
                NewCallResponse::Route(r)
            })
            .build(),
    ))
    .start(&h, "b2bua", "127.0.0.1:5649")
    .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, replaces")
        .through(b2bua.addr)
        .send()
        .await;

    let mut uas = bob.receive("INVITE").await;
    let supported = uas.request().header::<Supported>().expect("a Supported line").unwrap();
    assert_eq!(
        supported.iter().collect::<Vec<_>>(),
        ["timer"],
        "the declaration, minus the withheld tag"
    );

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
