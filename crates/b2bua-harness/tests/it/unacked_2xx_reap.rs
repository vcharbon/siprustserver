//! RFC 3261 §13.3.1.4 — an answered INVITE whose **2xx is never ACKed** must be
//! retransmitted (T1, doubling, capped T2) and, if no ACK arrives by 64·T1, the
//! UAS MUST clear the just-created dialog with a BYE.
//!
//! For the B2BUA this is a real latent leak: it relays bob's 200 to alice, but
//! the a-leg INVITE **server** transaction moves to `Completed` the moment the
//! final is sent, so the transaction layer never retransmits the 2xx
//! proactively (it only replays the stored 200 on a *retransmitted* INVITE), and
//! at Timer H the un-ACKed server txn is deleted **silently** — no BYE, no b-leg
//! teardown. The bridged, billable call then leaks until the 1 h GlobalDuration
//! cap (or bob's keepalive-timeout). `active_calls` stays pinned at 1.
//!
//! This scenario answers the call, then alice goes silent (never ACKs). The
//! RFC-correct B2BUA must:
//!   (a) **retransmit** the 2xx to alice at least once inside the ACK window, and
//!   (b) at the ACK-timeout deadline, **BYE the a-leg AND tear down the b-leg**
//!       (BYE to bob), driving `active_calls` back to 0.
//!
//! §13.2.2.4's "after acknowledging … MUST terminate with a BYE" orders (b) on
//! the callee face: a b-leg whose 2xx this stack can acknowledge alone gets the
//! ACK first, then the BYE. A delayed-offer b-leg gets the BYE alone — its ACK
//! owes the answer only the silent caller could supply.
//!
//! Paused-clock; the harness pins a short `ack_timeout_sec` so the give-up
//! deadline is reached in a handful of `advance`s (CLAUDE.md test-runtime policy:
//! cut churn at the source — the window, not real time).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, RunReport, WaiverScope};
use sip_message::{CustomParser, Method, SipMessage, SipParser};
use sip_retransmit::{Class, Ladder, Schedule};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The harness-pinned ACK-timeout (set via `tune` below). RFC's 64·T1 is 32 s;
/// the harness uses a compact window so the paused-clock advances stay cheap.
/// The give-up rule fires `ack_timeout_sec` after the 2xx is relayed.
const ACK_TIMEOUT_SEC: i64 = 6;

#[tokio::test(start_paused = true)]
async fn unacked_2xx_is_retransmitted_then_byes_both_legs() {
    let h = Harness::new("b2bua-unacked-2xx-reap");
    // ONE knowingly-unmet §13.2.2.4 obligation, and it is alice's: her silence IS
    // the reap under test. The b-leg keeps its own — the give-up composes bob's
    // ACK before the BYE, since the INVITE this stack sent him carried the offer.
    h.allow_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice deliberately never ACKs — the reap under test",
    );
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = b2bua_with_ack_timeout(&h, "b2bua", "127.0.0.1:5087", 5077, ACK_TIMEOUT_SEC).await;

    // ── Call setup: alice INVITEs, bob answers 200, but alice NEVER ACKs ──────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await; // alice receives the 200 — and deliberately stays silent.
    let b_leg_invite_cseq = uas.request().cseq().seq();
    let bob_tag = uas.dialog().local_tag().to_string();
    let b_leg_call_id = uas.request().call_id().as_str().to_string();

    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "exactly one active call after the (un-ACKed) answer",
    );

    // ── (a) The 2xx must be retransmitted to alice while her ACK is missing ──
    // Advance partway into the ACK window (past the first retransmit cadence) and
    // confirm at least one re-sent 200 reached alice.
    h.advance(Duration::from_secs(2)).await;
    let retransmits = alice.drain().await;
    assert!(
        retransmits >= 1,
        "RFC 3261 §13.3.1.4: the un-ACKed 2xx must be retransmitted to alice, got {retransmits}",
    );
    // Each rung that left is counted once, under the ladder that paced it and
    // the response it repeated: the two rungs inside 2 s (T1, then 2·T1).
    let counted = || b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200));
    assert_eq!(
        counted(),
        2,
        "b2bua_retransmits_total{{final-2xx,INVITE,200}} climbs with the rungs"
    );
    assert_eq!(counted(), retransmits as u64, "one increment per copy alice received");
    assert_eq!(b2bua.metrics().repeat_give_ups_total("ack-of-2xx"), 0, "no give-up yet");

    // ── (b) At the ACK-timeout deadline the B2BUA clears BOTH legs ───────────
    // Advance past the give-up deadline (armed when the 2xx was relayed). The
    // B2BUA BYEs the just-created a-leg dialog AND tears down the b-leg. The
    // caller (whose ACK was lost but is still reachable) and bob both answer, so
    // the call reaps without needing the 32 s Terminating safety net.
    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC as u64)).await;
    // Discard alice's accumulated (un-ACKed) 2xx retransmits so the next request
    // she receives is the give-up BYE (the BYE client txn retransmits, so one is
    // still in flight after the drain).
    alice.drain().await;
    // §13.2.2.4: "after acknowledging … MUST terminate with a BYE" — the callee
    // sees the ACK for his 2xx FIRST, bare, on that INVITE's CSeq and in his own
    // dialog, and the BYE after it.
    let give_up_ack = bob.receive("ACK").await;
    let ack = give_up_ack.request();
    assert!(ack.body().is_empty(), "the give-up ACK owes no answer: the offer was in the INVITE");
    assert_eq!(ack.cseq().seq(), b_leg_invite_cseq, "the ACK echoes the INVITE's CSeq");
    assert_eq!(ack.to().tag(), Some(bob_tag.as_str()), "in the callee's own dialog");
    assert_eq!(ack.call_id().as_str(), b_leg_call_id, "on the b-leg's Call-ID");
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    // The third rung (at 3.5 s) fell inside the 6 s bound; the fourth (7.5 s)
    // did not. The give-up is counted once, under the obligation left
    // undischarged.
    assert_eq!(counted(), 3, "every rung inside the bound, and none past it");
    assert_eq!(
        b2bua.metrics().repeat_give_ups_total("ack-of-2xx"),
        1,
        "one give-up: alice's ACK never came"
    );
    assert_eq!(b2bua.metrics().repeat_give_ups_total("prack-of"), 0);

    let _report = h.finish().await;
}

/// A deployment that waits longer than Timer L before acting on the silence.
const LATE_ACK_TIMEOUT_SEC: i64 = 60;

/// Every rung a `Final2xx` ladder owes inside Timer L, from the one schedule
/// the stack itself walks (ADR-0032 X1).
fn rungs_inside_timer_l() -> usize {
    let (mut ladder, _) =
        Ladder::armed(Schedule::rfc(Class::Final2xx)).expect("a 2xx owes its first re-send");
    let mut rungs = 1;
    while ladder.advance().is_some() {
        rungs += 1;
    }
    rungs
}

/// When each INVITE 2xx datagram left the SUT toward `caller`, in send order —
/// the original and every rung; a BYE's 200 never counts.
fn a_leg_invite_2xx_sent_ms(report: &RunReport, sut: SocketAddr, caller: SocketAddr) -> Vec<u64> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == caller)
        .filter(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r)) => {
                r.status() == 200 && *r.cseq().method() == Method::Invite
            }
            _ => false,
        })
        .map(|e| e.sent_ms)
        .collect()
}

/// RFC 3261 §13.3.1.4 ceases retransmitting at Timer L (64·T1) whatever the
/// deployment's ACK deadline says: a 60 s `ack_timeout_sec` waits longer
/// before tearing down, and adds no rung past 32 s. The two bounds are
/// distinct — the ladder's is protocol, the give-up's is policy.
#[tokio::test(start_paused = true)]
async fn a_deadline_past_timer_l_does_not_extend_the_2xx_ladder() {
    let h = Harness::new("b2bua-unacked-2xx-ceases-at-timer-l");
    h.allow_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice deliberately never ACKs — the ladder's bound under test",
    );
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let decision =
        Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", 5078));
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.ack_timeout_sec = LATE_ACK_TIMEOUT_SEC;
            // The subject is the 2xx ladder's bound against a late deadline; the
            // harness's 30 s keepalive would otherwise tear the call down first.
            c.keepalive_interval_sec = 600;
        })
        .start(&h, "b2bua", "127.0.0.1:5088")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await; // alice receives the 200 — and deliberately stays silent.

    // ── Past Timer L, well short of the deadline: the ladder has ceased and
    //    the deployment has not yet acted ──
    h.advance(Duration::from_secs(53)).await;
    alice.drain().await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "the call stands until the deployment's own deadline",
    );

    // ── The deadline (60 s): the deployment acts on the silence and clears
    //    both legs ──
    h.advance(Duration::from_secs(9)).await;
    alice.drain().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    let sent = a_leg_invite_2xx_sent_ms(&report, b2bua.addr, alice.addr());
    assert_eq!(
        sent.len(),
        1 + rungs_inside_timer_l(),
        "the original plus every rung the RFC prescribes inside Timer L, and none past it: {sent:?}",
    );
    assert_eq!(
        b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200)) as usize,
        rungs_inside_timer_l(),
        "the counter is the rungs on the wire — the original is not a repeat",
    );
    // Each rung fires on the next 100 ms tick of the paused clock, so the
    // tenth lands within about a second of its 31.5 s mark — and the eleventh,
    // due at 35.5 s, is never sent.
    let first = sent[0];
    let last = *sent.last().unwrap();
    assert!(
        last - first < 34_000,
        "the last copy leaves inside Timer L (+ tick slack), got {} ms after the first",
        last - first,
    );
}

/// ADR-0032 X5: the give-up is a deadline, not a switch. A config that writes a
/// non-positive `ack_timeout_sec` — bypassing `validate`, as a harness does —
/// still ends the session, at the 32 s default (Timer L).
#[tokio::test(start_paused = true)]
async fn a_nonpositive_deadline_still_ends_the_session_at_timer_l() {
    let h = Harness::new("b2bua-unacked-2xx-nonpositive-deadline");
    h.allow_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice deliberately never ACKs — the unconditional teardown under test",
    );
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let decision =
        Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", 5076));
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.ack_timeout_sec = 0;
            // The subject is the give-up's floor; the harness's 30 s keepalive
            // would otherwise tear the call down first.
            c.keepalive_interval_sec = 600;
        })
        .start(&h, "b2bua", "127.0.0.1:5086")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await; // alice receives the 200 — and deliberately stays silent.

    // Short of Timer L the call stands; at it the session ends on both legs.
    h.advance(Duration::from_secs(31)).await;
    alice.drain().await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "the call stands until the default deadline",
    );
    h.advance(Duration::from_secs(1)).await;
    alice.drain().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// A service that answers the un-ACKed 2xx give-up and deliberately declines to
/// end anything — it notes the silence in the CDR and parks the call.
mod parking {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceSeed,
    };
    use b2bua::{define_service, sm_rule};
    use call::{CdrEventType, Obligation, TimerType};

    define_service! {
        id: "parking",
        machine: PARKING,
        states: ParkingState { Parked },
        init: |_call: &RuleCall| Some(ServiceSeed::new(ParkingState::Parked.label())),
        rules: [ park_the_give_up() ],
    }

    fn is_2xx_give_up(ctx: &RuleContext) -> bool {
        matches!(
            ctx.timer_type(),
            Some(TimerType::RepeatGiveUp { obligation: Obligation::AckOf2xx { .. } })
        )
    }

    fn park_the_give_up() -> RuleDefinition {
        sm_rule! {
            id: "parking-declines-the-give-up",
            machine: PARKING,
            active: [ ParkingState::Parked ],
            transitions: [],
            effects: [],
            matcher: Match::timer().filter(is_2xx_give_up),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: ctx.call.a_leg().leg_id.clone(),
                    status_code: None,
                    reason: Some("parked".into()),
                }]))
            },
        }
    }
}

/// ADR-0032 X5: a service may re-author the give-up's teardown, not decline it.
/// With the parking service outranking `unacked-2xx-give-up`, the session still
/// ends at the deadline on both legs, under the CORE CDR marker.
#[tokio::test(start_paused = true)]
async fn a_service_that_parks_the_give_up_does_not_keep_the_session() {
    let h = Harness::new("b2bua-unacked-2xx-parked-give-up");
    h.allow_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice deliberately never ACKs — the give-up a service may not decline",
    );
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let decision =
        Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", 5075));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![parking::service_def()])
        .tune(|c| {
            c.ack_timeout_sec = ACK_TIMEOUT_SEC;
        })
        .start(&h, "b2bua", "127.0.0.1:5085")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await; // alice receives the 200 — and deliberately stays silent.

    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC as u64)).await;
    alice.drain().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let records = b2bua.cdr_records();
    assert_eq!(records.len(), 1, "one CDR");
    let reasons: Vec<&str> = records[0].events.iter().filter_map(|e| e.reason.as_deref()).collect();
    assert!(reasons.contains(&"parked"), "the service rule answered the give-up: {reasons:?}");
    assert!(
        reasons.contains(&"ack_timeout"),
        "and the framework still ended the session under the CORE marker: {reasons:?}",
    );

    let _report = h.finish().await;
}

/// The delayed-offer counter-case to the give-up ACK: alice's INVITE carried no
/// offer, so the ACK for bob's 2xx owes the answer only her own ACK supplies
/// (RFC 3264 §4). This stack cannot compose it, so bob gets the BYE alone and
/// his 2xx stays un-ACKed — his own §13.3.1.4 ladder is what covered the wait.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_callee_gets_the_give_up_bye_with_no_ack() {
    let h = Harness::new("b2bua-unacked-2xx-delayed-offer-give-up");
    // Both un-ACKed 2xx are the scenario: alice's silence leaves the answer
    // relayed to her un-ACKed, and the callee's delayed-offer 2xx can only be
    // acknowledged by her own ACK (RFC 3264 §4), which never came.
    h.waive(
        WaiverScope::rule(
            "no-ack-to-dialog-creating-2xx",
            "the caller deliberately never ACKs, so neither her answer nor the callee's \
             delayed-offer 2xx — whose ACK owes the answer only she could supply — is \
             acknowledged; that silence is the give-up under test",
        )
        .on_party("b2bua"),
    );
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = b2bua_with_ack_timeout(&h, "b2bua", "127.0.0.1:5084", 5074, ACK_TIMEOUT_SEC).await;

    // ── alice INVITEs bodyless: the offer is bob's to make ───────────────────
    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(uas.request().body().is_empty(), "the offerless INVITE reached bob with a body");
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await; // alice receives the offer — and deliberately stays silent.

    // ── At the deadline: the BYE, and only the BYE, reaches the callee ───────
    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC as u64)).await;
    alice.drain().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    let acks = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob.addr() && e.raw.starts_with(b"ACK "))
        .count();
    assert_eq!(acks, 0, "a delayed-offer b-leg cannot be ACKed by this stack");
}

/// A B2BUA that routes every call to `dest_port` with a short `ack_timeout_sec`
/// so the un-ACKed-2xx give-up deadline is reached in a few paused-clock steps.
async fn b2bua_with_ack_timeout(
    h: &Harness,
    name: &str,
    addr: &str,
    dest_port: u16,
    ack_timeout_sec: i64,
) -> B2buaSut {
    let decision =
        Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", dest_port));
    B2buaSut::builder(decision)
        .tune(move |c| {
            c.ack_timeout_sec = ack_timeout_sec;
        })
        .start(h, name, addr)
        .await
}
