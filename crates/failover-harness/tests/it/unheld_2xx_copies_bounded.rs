//! **A 2xx no call here holds, re-sent on every ACK.** A final that matches no
//! transaction and no leg a node holds is answered from its own headers (ADR-0055
//! §4, §12): a 2xx ACKed and its dialog BYEd (RFC 3261 §13.2.2.4, §15.1.1). Its
//! answerer re-sends it on §13.3.1.4's schedule only, so the ACK follows ADR-0057
//! §10's rule and the BYE leaves once (ADR-0063 §2): a copy past the schedule's
//! count, arriving within T1 of the last ACK, is absorbed.
//!
//! The callee is broken on purpose: it re-sends its 2xx on every ACK it
//! receives, so each answer draws a new copy at once. The node answers the
//! copies a compliant answerer can send, one BYE in all, and the ping-pong
//! stops. The callee answers each BYE into its ended dialog with a 481 (ADR-0055
//! §11). The killed primary returns and discharges the call's one CDR.
//!
//! - **A vanished call.** The primary dies with a re-INVITE round in no replica;
//!   the callee's 2xx to it takes the call over on the survivor, which ACKs it,
//!   ends the call and releases its copy. The callee then re-sends that 2xx.
//! - **A held call's unknown leg.** The callee REFERs the caller to a target the
//!   primary dials; the primary dies with that dial in no replica. The target's
//!   2xx takes the call over on the survivor, whose record lacks the leg its
//!   via-extension names (ADR-0055 §4, trigger 5): it ACKs and BYEs the target and
//!   ends the call. The target re-sends its 2xx while the callee holds its BYE
//!   unanswered, so the call is held throughout.
//!
//! Each round sends one copy, then reads after a settled advance of a few
//! transits: the verdict depends on no in-order draining of the paused runtime.
//!
//! Beyond the failover sweep: a broken peer's 2xx loop against the unheld answer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{default_call_refer, route_to_processing_refer};
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use failover_harness::{FailoverHarness, PartitionRole};
use scenario_harness::agent::Agent;
use scenario_harness::Inbound;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::call_terminate_on_backup::{ANSWER, OFFER};
use crate::departure_support::{bring_up_deciding, kill_announced, placed, CountedDecision};
use crate::inflight_resend::{
    reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of, REANSWER, REPLICATE,
};
use crate::reinvite_round_not_yet_replicated::lose_a_round;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const CHARLIE: &str = "127.0.0.1:5090";

/// The settled advance after each copy, twice: the copy's two hops through the
/// proxy, the answers' two hops back, the callee's 481 to a BYE, and slack for
/// the node's turns. Far inside T1.
const ROUND: Duration = Duration::from_millis(20);
/// The test's own bound on the ping-pong: a node that never stops answering
/// ends the loop here, never the wall clock: far past what a compliant
/// answerer sends, its rounds inside 64·T1.
const ROUNDS: usize = 100;
/// The most copies of one 2xx a compliant answerer sends after the first (RFC
/// 3261 §13.3.1.4, T1 = 500 ms, T2 = 4 s; ADR-0057 §10).
const COMPLIANT_COPIES: usize = 10;

/// What the node put on the wire toward the callee while it repeated its 2xx.
struct PingPong {
    /// The copies of the 2xx the callee sent.
    copies: usize,
    /// The ACKs that reached the callee.
    acks: usize,
    /// The BYEs that reached the callee, each answered 481.
    byes: usize,
}

impl PingPong {
    /// The node ACKs no more copies than a compliant answerer sends (the first
    /// arrival and the [`COMPLIANT_COPIES`] after it), absorbs the copy that
    /// ends the ping-pong, and BYEs the dialog once.
    fn assert_bounded(&self) {
        assert!(
            (1..=COMPLIANT_COPIES + 1).contains(&self.acks),
            "the node ACKs at most the {} copies a compliant answerer sends (RFC 3261 \
             §13.3.1.4), got {} ACKs over {} copies",
            COMPLIANT_COPIES + 1,
            self.acks,
            self.copies,
        );
        assert_eq!(self.copies, self.acks + 1, "the first copy left un-ACKed ends the ping-pong");
        assert_eq!(self.byes, 1, "the dialog the 2xx names is BYEd once (ADR-0063 §2)");
    }
}

/// The last 2xx to an INVITE `callee` sent, as it left, and where it went.
fn last_invite_2xx(fh: &FailoverHarness, callee: SocketAddr) -> (Vec<u8>, SocketAddr) {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.from == callee)
        .rfind(|e| {
            matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r))
                if (200..300).contains(&r.status()) && r.cseq().method() == Method::Invite)
        })
        .map(|e| (e.raw, e.to))
        .expect("the callee's 2xx is on the wire")
}

/// The ACKs and the BYEs that reached `callee` in the wire entries past `from`.
fn reached(fh: &FailoverHarness, callee: SocketAddr, from: usize) -> (usize, usize) {
    let to_callee: Vec<_> =
        fh.sip_entries().into_iter().skip(from).filter(|e| e.to == callee).collect();
    let acks = to_callee.iter().filter(|e| e.raw.starts_with(b"ACK ")).count();
    let byes = to_callee.iter().filter(|e| e.raw.starts_with(b"BYE ")).count();
    (acks, byes)
}

/// Answers every BYE queued at `callee` with a 481 (its dialog is ended) and
/// absorbs the rest.
async fn refuse_byes(callee: &Agent) {
    while let Some(inbound) = callee.take_queued_inbound().await {
        let Inbound::Request(mut txn) = inbound else { continue };
        if txn.request().method() == "BYE" {
            txn.respond(481, "Call/Transaction Does Not Exist").await;
        }
    }
}

/// Plays the broken callee: re-sends `copy` toward `to` and waits one round,
/// again while that round drew anything, for at most [`ROUNDS`] copies. The
/// count runs from the wire entry `since`, after which the callee had sent
/// `sent` copies already.
async fn ping_pong(
    fh: &FailoverHarness,
    callee: &Agent,
    (copy, to): (&[u8], SocketAddr),
    (since, sent): (usize, usize),
) -> PingPong {
    let mut copies = sent;
    for _ in 0..ROUNDS {
        let round = fh.sip_entries().len();
        callee.try_send_datagram(copy, to).await.expect("the copy leaves");
        copies += 1;
        fh.advance(ROUND).await;
        refuse_byes(callee).await;
        fh.advance(ROUND).await;
        if reached(fh, callee.addr(), round) == (0, 0) {
            break;
        }
    }
    let (acks, byes) = reached(fh, callee.addr(), since);
    PingPong { copies, acks, byes }
}

/// Routes every call to bob and lets bob's REFER dial the target it names.
fn transferring_engine() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(route_to_processing_refer("127.0.0.1", 5070)))
            .on_refer(default_call_refer)
            .build(),
    )
}

#[tokio::test(start_paused = true)]
async fn a_vanished_calls_2xx_repeated_on_every_ack_draws_bounded_acks_and_one_bye() {
    let mut lost = lose_a_round("unheld-2xx-copies-bounded-vanished").await;
    lost.at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    let answer = last_invite_2xx(&lost.fh, lost.bob.addr());
    lost.bob.receive("ACK").await;
    lost.taken_over_once("the callee's 200");
    lost.ended_on_both_legs().await;
    lost.fh.strike(failover_harness::Incident::PrimaryKilled);
    let released = lost
        .fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            !lost.survivor().serves(&lost.call_ref)
        })
        .await;
    assert!(released, "the survivor ends the call and releases its copy");

    let since = lost.fh.sip_entries().len();
    ping_pong(&lost.fh, &lost.bob, (&answer.0, answer.1), (since, 0)).await.assert_bounded();

    lost.settle().await;
}

/// The dial to the target never lands at the backup: the backup's replica is
/// rewound to the call as it was before the REFER when the primary dies.
#[tokio::test(start_paused = true)]
async fn a_held_calls_unknown_leg_2xx_repeated_on_every_ack_draws_bounded_acks_and_one_bye() {
    let mut fh = FailoverHarness::new("unheld-2xx-copies-bounded-unknown-leg", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let charlie = fh.agent("charlie", CHARLIE).await.transfer_target();
    let (decision, _consults) = CountedDecision::counting(transferring_engine(), Duration::ZERO);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = at_bob.dialog();
    fh.advance(Duration::from_secs(1)).await;
    let established = survivor_of(&pri_ord, &w_b1, &w_b2)
        .get(PartitionRole::Backup, &pri_ord, &call_ref)
        .await
        .expect("the established call's replica");

    let api = r#"{"refer_key":"refer-allow-c","destination":{"host":"127.0.0.1","port":5090}}"#;
    bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@{CHARLIE}>"))
        .with_header("X-Api-Call", api)
        .send()
        .await
        .expect(202)
        .await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut at_charlie = charlie.receive("INVITE").await;
    at_charlie.respond(180, "Ringing").await;
    fh.advance(REPLICATE).await;
    survivor_of(&pri_ord, &w_b1, &w_b2)
        .rewind_replica(PartitionRole::Backup, &pri_ord, &call_ref, established)
        .await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.strike(failover_harness::Incident::PrimaryKilled);

    let since = fh.sip_entries().len();
    at_charlie.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = last_invite_2xx(&fh, charlie.addr());
    charlie.receive("ACK").await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    alice.receive("BYE").await.respond(200, "OK").await;
    let mut bobs_bye = bob.receive_tolerating("BYE", &["NOTIFY"]).await;

    ping_pong(&fh, &charlie, (&answer.0, answer.1), (since, 1)).await.assert_bounded();
    bobs_bye.respond(200, "OK").await;

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob, &charlie], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
