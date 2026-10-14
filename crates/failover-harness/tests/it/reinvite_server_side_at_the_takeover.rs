//! **The caller's side of a re-INVITE round the dead node relayed.** The
//! round is in the replica when the serving node dies; the survivor rebuilds
//! the caller's INVITE server transaction from the record, so whatever the
//! caller sends on that transaction next is answered as the dead node would
//! have answered it.
//!
//! ```text
//!   answered:   alice ─INVITE─▶ primary ─INVITE─▶ bob ─200─▶ primary ─200─╳ alice
//!               ✗ primary killed ✗   alice re-sends the INVITE (Timer A)
//!               alice ─INVITE─▶ SURVIVOR ─200 (the dead node's)─▶ alice
//!
//!   in flight:  alice ─INVITE─▶ primary ─INVITE─▶ bob ─100─▶ primary
//!               ✗ primary killed ✗   alice CANCELs (§9.1)
//!               alice ─CANCEL─▶ SURVIVOR ─200, 487─▶ alice, ─CANCEL─▶ bob
//! ```
//!
//! An answered round's transaction is in RFC 6026 Accepted: a copy of the
//! INVITE draws the 2xx the record retains, never a 491 beside it. An
//! unanswered round's transaction holds its INVITE, rebuilt from the record:
//! the caller's CANCEL draws 200 and 487 and the survivor CANCELs the callee's
//! side of the round (RFC 3261 §9.2). Each comes two ways: the caller's own
//! message takes the call over, or another request took it over first and the
//! caller's message meets the rebuilt transaction already in place.
//!
//! The kill is never told to the peers (ADR-0031 cases 3a and 4). Every cell
//! ends with a BYE, one CDR and clean memory on both nodes.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::FailoverHarness;
use scenario_harness::agent::Agent;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::departure_support::kill_announced;
use crate::inflight_resend::{
    assert_not_taken_over, assert_taken_over_once, bring_up, establish, kill_serving_node,
    last_request_sent, reboot_and_reclaim, round_invites_sent_to, settle_to_one_cdr, split_mut,
    survivor_of, synchronized_call_ref, KILL_SETTLE, REANSWER, REOFFER, REPLICATE, T1,
};
use crate::reinvite_2xx_acked_through_the_primary::re_ack_copies;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The responses with `status` to a `method` request put on the wire toward
/// `to`.
fn responses_toward(fh: &FailoverHarness, to: SocketAddr, method: &Method, status: u16) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter(|e| {
            matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r))
                if r.cseq().method() == method && r.status() == status)
        })
        .count()
}

/// Everything queued at `party`, as `(CSeq method, status)` per response;
/// a request is a failure.
async fn queued_responses(party: &Agent) -> Vec<(Method, u16)> {
    let mut out = Vec::new();
    while let Some(msg) = party.take_queued().await {
        match msg {
            SipMessage::Response(r) => out.push((r.cseq().method().clone(), r.status())),
            SipMessage::Request(r) => panic!("unexpected {} toward {}", r.method(), party.name()),
        }
    }
    out
}

/// A re-INVITE whose 2xx replicated, the 2xx lost on the way to the caller,
/// retransmitted by the caller after the kill: the retransmission takes the
/// call over and draws the 2xx the record retains, never a 491; the caller's
/// ACK reaches the callee once.
#[tokio::test(start_paused = true)]
async fn a_reinvite_retransmitted_after_its_2xx_replicated_draws_that_2xx_at_the_takeover() {
    let mut fh = FailoverHarness::new("reinvite-2xx-retransmitted-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── Answered and replicated; the 2xx lost on the way to the caller ──────
    let reinvite = dialog.send_request(InDialogMethod::Invite).with_sdp(REOFFER).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut at_bob = bob.receive("INVITE").await;
    let cseq = reinvite.sent_invite().expect("the re-INVITE").cseq().seq();
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("alice", alice.addr());
    // The T1 rung fell inside the cut; the 3·T1 rung is the first after the kill.
    fh.advance(3 * T1 - REPLICATE - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── The caller's Timer A rung takes the call over ───────────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    let seen = queued_responses(&alice).await;
    assert!(
        seen.contains(&(Method::Invite, 200)),
        "the retransmission draws the retained 2xx: {seen:?}"
    );
    assert_eq!(
        responses_toward(&fh, alice.addr(), &Method::Invite, 491),
        0,
        "no 491 beside the retained 2xx (RFC 3261 §17.2.1)",
    );
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), REOFFER), 1, "one b-leg INVITE");

    // ── The caller ACKs; the callee draws one ACK ───────────────────────────
    dialog.ack_for(cseq, None).await;
    let acks = re_ack_copies(&fh, &alice, &mut dialog, &bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    drop(reinvite);

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The same answered round, but the caller's INFO takes the call over first:
/// the rebuilt transaction is already in place when the caller's INVITE
/// retransmission, sent before the retained 2xx reaches her, arrives, and it
/// absorbs the retransmission with that 2xx — no 100, no 491.
#[tokio::test(start_paused = true)]
async fn a_reinvite_retransmitted_after_another_takeover_draws_its_replicated_2xx() {
    let mut fh = FailoverHarness::new("reinvite-2xx-retransmitted-after-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    let reinvite = dialog.send_request(InDialogMethod::Invite).with_sdp(REOFFER).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut at_bob = bob.receive("INVITE").await;
    let cseq = reinvite.sent_invite().expect("the re-INVITE").cseq().seq();
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("alice", alice.addr());
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── An INFO takes the call over; the INVITE rung crosses the 2xx ────────
    let info = dialog.send_request(InDialogMethod::Info).send().await;
    fh.advance(Duration::from_millis(3)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    bob.receive("INFO").await.respond(200, "OK").await;
    fh.advance(Duration::from_millis(200)).await;
    let seen = queued_responses(&alice).await;
    assert!(seen.contains(&(Method::Info, 200)), "the INFO is answered: {seen:?}");
    assert!(seen.contains(&(Method::Invite, 200)), "the retained 2xx reaches her: {seen:?}");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2).sip_addr();
    assert_eq!(
        responses_from_toward(&fh, survivor, proxy.addr(), &Method::Invite, 100),
        0,
        "the rebuilt transaction absorbs the retransmission: no 100, nothing opens beside it",
    );
    assert_eq!(responses_toward(&fh, alice.addr(), &Method::Invite, 491), 0, "no 491");
    drop((info, reinvite));

    dialog.ack_for(cseq, None).await;
    let acks = re_ack_copies(&fh, &alice, &mut dialog, &bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The same answered round, the caller having heard the 100 so she sends
/// nothing more, the 2xx lost on its way to her; the serving node's
/// departure is announced and no message for the call follows. The departure
/// sweep takes the call over and resumes the retained 2xx's ladder (RFC 3261
/// §13.3.1.4): the 2xx reaches the caller and her ACK reaches the callee once.
#[tokio::test(start_paused = true)]
async fn a_retained_2xx_is_retransmitted_by_the_survivor_after_its_nodes_departure() {
    let mut fh = FailoverHarness::new("reinvite-2xx-resumed-on-departure", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── Answered and replicated; the 2xx lost on the way to the caller ──────
    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    fh.cut_signalling("alice", alice.addr());
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("alice", alice.addr());

    // ── The survivor's ladder brings the 2xx to the caller ──────────────────
    reinvite.expect(200).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    dialog.ack(None).await;
    let acks = re_ack_copies(&fh, &alice, &mut dialog, &bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The responses with `status` to a `method` request `from` put on the wire
/// toward `to`.
fn responses_from_toward(
    fh: &FailoverHarness,
    from: SocketAddr,
    to: SocketAddr,
    method: &Method,
    status: u16,
) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.from == from && e.to == to)
        .filter(|e| {
            matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r))
                if r.cseq().method() == method && r.status() == status)
        })
        .count()
}

/// Which request reaches the survivor first after the kill.
#[derive(Clone, Copy)]
enum FirstAtTheSurvivor {
    /// The caller's CANCEL takes the call over itself.
    Cancel,
    /// The caller's INFO takes the call over; the CANCEL follows.
    Info,
}

/// A re-INVITE relayed and replicated, the callee's 100 its only answer, the
/// caller's 100 heard: after the kill the caller CANCELs it. The survivor
/// answers 200 and 487 from the transaction it rebuilt, CANCELs the callee's
/// side once the callee's transaction repeats its 100, ACKs the callee's 487,
/// and the call continues to its BYE.
async fn reinvite_cancelled_at_the_takeover(name: &str, first: FirstAtTheSurvivor) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── In flight and replicated; the caller heard the 100 ─────────────────
    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    if let FirstAtTheSurvivor::Info = first {
        let mut info = dialog.send_request(InDialogMethod::Info).send().await;
        bob.receive("INFO").await.respond(200, "OK").await;
        info.expect(200).await;
        fh.advance(Duration::from_millis(200)).await;
        assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    }

    // ── The caller gives the round up ───────────────────────────────────────
    let mut cancel = reinvite.cancel().await;
    cancel.expect(200).await;
    reinvite.expect(487).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    // ── The callee's side: the INVITE re-sent once, its 100, the CANCEL ─────
    let resent = bob.receive("INVITE").await;
    assert_eq!(resent.request().top_via().branch(), at_bob.request().top_via().branch());
    at_bob.respond(100, "Trying").await;
    bob.receive_absorbing("CANCEL", &["INVITE"]).await.respond(200, "OK").await;
    at_bob.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), REOFFER), 1, "one b-leg INVITE");

    // ── The call lives on and is hung up normally ───────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_cancelled_at_the_takeover_draws_200_and_487() {
    reinvite_cancelled_at_the_takeover("reinvite-cancelled-takeover", FirstAtTheSurvivor::Cancel)
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_cancelled_after_another_takeover_draws_200_and_487() {
    reinvite_cancelled_at_the_takeover(
        "reinvite-cancelled-after-takeover",
        FirstAtTheSurvivor::Info,
    )
    .await;
}
