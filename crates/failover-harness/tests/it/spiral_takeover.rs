//! A spiral (RFC 3261 §16.3) across a worker kill: call 1's outgoing INVITE
//! leaves through the front proxy to a third-party proxy, which sends it back
//! through the front proxy as call 2, and both calls run on one worker. That
//! worker is killed while the calls ring, or once they are confirmed; the
//! survivor takes both over and must give every request of the leg between
//! the two calls, which carries one Call-ID and From tag for both, to the
//! right call: the CANCEL and the re-INVITE to the call that received the
//! leg (call 2), their responses and the ACK of a non-2xx final to the side
//! each belongs to.
//!
//!   alice ─▶ LB ─▶ worker (call 1) ─▶ LB ─▶ proxy ─▶ LB ─▶ worker (call 2) ─▶ LB ─▶ bob
//!
//! The front proxy places a new call by the hash of its Call-ID, so call 2
//! lands on call 1's worker only for some of alice's Call-IDs: the setup tries
//! alice's Call-IDs in turn and rejects the attempts that split the calls.
//! Each cell runs through a third party that Record-Routes and one that does
//! not (§16.6 step 4); the front proxy Record-Routes both of its passes either
//! way, so in-dialog requests reach the survivor by its cookies.
//! The killed worker reboots and reclaims, as the sole CDR authority of both
//! calls (ADR-0020 X3): each call then has exactly one CDR in the cluster.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::CdrRecord;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use call::CdrEventType;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, ClientInvite, Dialog, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::stateful_proxy::{stateful_proxy_on, RecordRoute, StatefulProxy};

const ALICE: &str = "127.0.0.1:5060";
const BOB_PORT: u16 = 5070;
const BOB: &str = "127.0.0.1:5070";
const LB: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const THIRD_PARTY_PORT: u16 = 5095;
/// The Request-URI call 1 sends to the third party; coming back, it names
/// call 2's route.
const SPIRAL_USER: &str = "spiral";
/// Alice's Call-IDs tried before giving up on placing both calls on one
/// worker; each try lands both on one worker with probability one half.
const PLACEMENT_TRIES: usize = 16;

const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=sendonly\r\n";
const BOB_REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=recvonly\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=sendonly\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=recvonly\r\n";

/// Call 1 goes to the third party; call 2 (the spiralled Request-URI) goes
/// to bob.
fn spiral_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .on(|req| {
                req.ruri.starts_with(&format!("sip:{SPIRAL_USER}@")).then(|| {
                    let mut r = route_to("127.0.0.1", BOB_PORT);
                    r.new_ruri = Some(format!("sip:bob@{BOB}"));
                    NewCallResponse::Route(r)
                })
            })
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", THIRD_PARTY_PORT);
                r.new_ruri = Some(format!("sip:{SPIRAL_USER}@127.0.0.1:{THIRD_PARTY_PORT}"));
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The cluster with both spiralled calls on one worker, bob's INVITE in hand.
struct Spiral {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    lb: ProxySut,
    _third_party: StatefulProxy,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    /// The worker holding both calls.
    primary: String,
    call: ClientInvite,
    uas: ServerTxn,
}

impl Spiral {
    async fn new(name: &str, record_route: RecordRoute) -> Self {
        let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
        let alice = fh.agent("alice", ALICE).await;
        let bob = fh.agent("bob", BOB).await;
        let lb =
            fh.spawn_proxy(LB, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
        let third_party = stateful_proxy_on(
            fh.agent_with_roles(
                "third-party",
                &format!("127.0.0.1:{THIRD_PARTY_PORT}"),
                HashSet::from([sip_net::UaRole::Proxy]),
            )
            .await,
            record_route,
            Some(lb.addr()),
        );
        let mut workers = Vec::new();
        for (ordinal, bind, peer) in [("b1", B1, "b2"), ("b2", B2, "b1")] {
            workers.push(
                fh.spawn_worker_limited(
                    ordinal,
                    ordinal,
                    bind,
                    &[peer],
                    ("127.0.0.1", BOB_PORT),
                    ("127.0.0.1", 5080),
                    spiral_decision(),
                    Arc::new(NoopLimiter),
                )
                .await,
            );
        }
        let (w_b2, w_b1) = (workers.pop().unwrap(), workers.pop().unwrap());
        fh.advance(Duration::from_millis(500)).await;
        assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready");

        for attempt in 0..PLACEMENT_TRIES {
            let mut call = alice
                .invite(&bob)
                .identity(format!("spiral-ha-{attempt}@127.0.0.1"), format!("alice-{attempt}"))
                .with_sdp(OFFER_SDP)
                .through(lb.addr())
                .send()
                .await;
            let mut uas = bob.receive("INVITE").await;
            let (primary, _) = worker_ordinals(uas.request());
            let worker = if primary == "b1" { &w_b1 } else { &w_b2 };
            if worker.active_calls() == 2 {
                return Self {
                    fh,
                    alice,
                    bob,
                    lb,
                    _third_party: third_party,
                    w_b1,
                    w_b2,
                    primary,
                    call,
                    uas,
                };
            }
            // The calls split across the workers: end this attempt.
            uas.respond(486, "Busy Here").await;
            uas.expect_ack().await;
            call.expect(486).await;
            assert_split_attempt_recorded(&fh, &[&w_b1, &w_b2], &call.call_id()).await;
        }
        panic!("no Call-ID among {PLACEMENT_TRIES} placed both spiralled calls on one worker");
    }

    /// Bob rings and alice sees it.
    async fn ring(&mut self) {
        self.uas.respond(180, "Ringing").await;
        self.call.expect(180).await;
    }

    /// Bob answers; alice ACKs. Returns (alice's dialog, bob's dialog).
    async fn answer(&mut self) -> (Dialog, Dialog) {
        self.uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        self.call.expect(200).await;
        let alice = self.call.ack().await;
        self.bob.receive("ACK").await;
        (alice, self.uas.dialog())
    }

    /// The two calls the worker holds, once replicated to the survivor.
    async fn replicated_calls(&self) -> Vec<String> {
        let survivor = self.survivor();
        let mut refs = Vec::new();
        for _ in 0..50 {
            self.fh.advance(Duration::from_millis(100)).await;
            refs = survivor.scan_backed_up(&self.primary);
            refs.retain(|r| self.primary_node().serves(r));
            if refs.len() == 2 {
                break;
            }
        }
        assert_eq!(refs.len(), 2, "both live calls replicated to the survivor: {refs:?}");
        refs
    }

    fn primary_node(&self) -> &ReplicatedB2buaSut {
        if self.primary == "b1" {
            &self.w_b1
        } else {
            &self.w_b2
        }
    }

    fn survivor(&self) -> &ReplicatedB2buaSut {
        if self.primary == "b1" {
            &self.w_b2
        } else {
            &self.w_b1
        }
    }

    /// Kill the worker holding both calls, with the complete death signal:
    /// dead at the front proxy, gone from the survivor's membership.
    async fn kill_the_worker(&mut self) {
        self.fh.mark(&self.primary, None, "crash", "the worker holding both looped calls");
        let (primary, survivor) = if self.primary == "b1" {
            (&mut self.w_b1, &self.w_b2)
        } else {
            (&mut self.w_b2, &self.w_b1)
        };
        primary.crash();
        self.lb.set_health(&self.primary, WorkerHealth::Dead);
        survivor.simulate_peer_removed(&self.primary);
        self.fh.advance(Duration::from_millis(300)).await;
    }

    /// The killed worker reboots and reclaims; both calls end with one CDR
    /// each and no trace anywhere; the trace passes the RFC audit. Returns
    /// the two calls' CDRs.
    async fn reboot_and_finish(mut self, calls: &[String]) -> Vec<CdrRecord> {
        self.fh.advance(Duration::from_secs(60)).await;
        let ordinal = self.primary.clone();
        self.fh.mark(&ordinal, None, "reboot", "restart empty, higher gen, new pod IP");
        let (primary, survivor) = if ordinal == "b1" {
            (&mut self.w_b1, &self.w_b2)
        } else {
            (&mut self.w_b2, &self.w_b1)
        };
        let new_addr = primary.reboot().await;
        self.lb.set_address(&ordinal, new_addr);
        self.fh.note_worker_rebound(&ordinal, new_addr);
        survivor.simulate_peer_added(&ordinal);
        for _ in 0..120 {
            self.fh.advance(Duration::from_millis(500)).await;
            if primary.is_ready() {
                break;
            }
        }
        assert!(primary.is_ready(), "the rebooted worker became ready");
        self.lb.set_health(&ordinal, WorkerHealth::Alive);
        self.fh.advance(Duration::from_secs(10)).await;

        let (w_b1, w_b2) = (&self.w_b1, &self.w_b2);
        let _ = self
            .fh
            .settle_terminal(async || {
                let mut over = w_b1.memory_clean() && w_b2.memory_clean();
                for c in calls {
                    over = over && !w_b1.holds_any_trace(c).await && !w_b2.holds_any_trace(c).await;
                }
                over
            })
            .await;
        self.fh.linger_peers(&[&self.alice, &self.bob], Duration::from_secs(3)).await;
        for c in calls {
            assert_eq!(total_cdrs_for(&[w_b1, w_b2], c), 1, "exactly one CDR for {c}");
            assert_call_fully_released(&[w_b1, w_b2], c).await;
        }
        self.fh.assert_sip_rfc_clean("spiral-takeover");
        [w_b1, w_b2]
            .iter()
            .flat_map(|n| n.cdr_records())
            .filter(|r| calls.contains(&r.call_ref))
            .collect()
    }
}

/// The two calls of a rejected placement attempt, alice's and the one her
/// call's outgoing leg spiralled into, each end with one CDR, unanswered.
async fn assert_split_attempt_recorded(
    fh: &FailoverHarness,
    nodes: &[&ReplicatedB2buaSut],
    alice_call_id: &str,
) {
    let cdrs = || nodes.iter().flat_map(|n| n.cdr_records()).collect::<Vec<_>>();
    let recorded = |cdrs: &[CdrRecord]| {
        let first: Vec<_> = cdrs.iter().filter(|c| c.a_leg.call_id == alice_call_id).collect();
        let [first] = first.as_slice() else { return None };
        let looped = &first.b_legs.first()?.call_id;
        let second: Vec<_> = cdrs.iter().filter(|c| &c.a_leg.call_id == looped).collect();
        let [second] = second.as_slice() else { return None };
        Some([(*first).clone(), (*second).clone()])
    };
    for _ in 0..50 {
        if recorded(&cdrs()).is_some() {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    let pair = recorded(&cdrs()).expect("one CDR for each call of the split attempt");
    for cdr in &pair {
        let k = kinds(cdr);
        assert!(!k.contains(&CdrEventType::Answer), "a rejected attempt is unanswered: {k:?}");
    }
}

fn kinds(cdr: &CdrRecord) -> Vec<CdrEventType> {
    cdr.events.iter().map(|e| e.event_type).collect()
}

/// Neither call was answered; each records how it ended.
fn assert_unanswered(cdrs: &[CdrRecord], ended: CdrEventType) {
    assert_eq!(cdrs.len(), 2, "one CDR per spiralled call: {cdrs:?}");
    for cdr in cdrs {
        let k = kinds(cdr);
        assert!(k.contains(&ended) && !k.contains(&CdrEventType::Answer), "{k:?}");
    }
}

/// Both calls were answered and ended by a BYE.
fn assert_answered_and_ended(cdrs: &[CdrRecord]) {
    assert_eq!(cdrs.len(), 2, "one CDR per spiralled call: {cdrs:?}");
    for cdr in cdrs {
        let k = kinds(cdr);
        assert!(k.contains(&CdrEventType::Answer) && k.contains(&CdrEventType::Bye), "{k:?}");
    }
}

/// `dialog`'s end re-INVITEs with `offer` after the kill; the survivor relays
/// it across both calls and the far end answers.
async fn reinvite_across(dialog: &mut Dialog, far: &Agent, offer: &str, answer: &str) {
    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(offer)).await;
    let mut uas = far.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(answer).await;
    reinvite.expect(200).await;
    dialog.ack(None).await;
    far.receive("ACK").await;
}

/// Confirmed, then killed: alice re-INVITEs and hangs up on the survivor.
#[tokio::test(start_paused = true)]
async fn a_confirmed_spiral_survives_a_worker_kill_caller_reinvite_and_bye() {
    confirmed_caller_reinvite_and_bye("spiral-takeover-confirmed-caller", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_spiral_survives_a_worker_kill_caller_reinvite_and_bye_without_record_route() {
    confirmed_caller_reinvite_and_bye("spiral-takeover-confirmed-caller-no-rr", RecordRoute::No)
        .await;
}

async fn confirmed_caller_reinvite_and_bye(name: &str, record_route: RecordRoute) {
    let mut sp = Spiral::new(name, record_route).await;
    sp.ring().await;
    let (mut alice, _bob) = sp.answer().await;
    let calls = sp.replicated_calls().await;
    sp.kill_the_worker().await;

    reinvite_across(&mut alice, &sp.bob, ALICE_REOFFER, BOB_REANSWER).await;
    scenario_harness::callflow::hangup(&mut alice, &sp.bob).await;
    assert_answered_and_ended(&sp.reboot_and_finish(&calls).await);
}

/// Confirmed, then killed: bob re-INVITEs and hangs up on the survivor.
#[tokio::test(start_paused = true)]
async fn a_confirmed_spiral_survives_a_worker_kill_callee_reinvite_and_bye() {
    confirmed_callee_reinvite_and_bye("spiral-takeover-confirmed-callee", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_spiral_survives_a_worker_kill_callee_reinvite_and_bye_without_record_route() {
    confirmed_callee_reinvite_and_bye("spiral-takeover-confirmed-callee-no-rr", RecordRoute::No)
        .await;
}

async fn confirmed_callee_reinvite_and_bye(name: &str, record_route: RecordRoute) {
    let mut sp = Spiral::new(name, record_route).await;
    sp.ring().await;
    let (_alice, mut bob) = sp.answer().await;
    let calls = sp.replicated_calls().await;
    sp.kill_the_worker().await;

    reinvite_across(&mut bob, &sp.alice, BOB_REOFFER, ALICE_REANSWER).await;
    scenario_harness::callflow::hangup(&mut bob, &sp.alice).await;
    assert_answered_and_ended(&sp.reboot_and_finish(&calls).await);
}

/// Confirmed, then killed: alice's re-INVITE is rejected 488 by bob; each
/// hop ACKs the 488 below it, the third party's ACK reaching call 2 on the
/// survivor, and the call goes on to a BYE.
#[tokio::test(start_paused = true)]
async fn a_confirmed_spiral_survives_a_worker_kill_rejected_reinvite() {
    confirmed_rejected_reinvite("spiral-takeover-confirmed-488", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_spiral_survives_a_worker_kill_rejected_reinvite_without_record_route() {
    confirmed_rejected_reinvite("spiral-takeover-confirmed-488-no-rr", RecordRoute::No).await;
}

async fn confirmed_rejected_reinvite(name: &str, record_route: RecordRoute) {
    let mut sp = Spiral::new(name, record_route).await;
    sp.ring().await;
    let (mut alice, _bob) = sp.answer().await;
    let calls = sp.replicated_calls().await;
    sp.kill_the_worker().await;

    let mut reinvite = alice.request(InDialogMethod::Invite, Some(ALICE_REOFFER)).await;
    let mut uas = sp.bob.receive("INVITE").await;
    uas.respond(488, "Not Acceptable Here").await;
    uas.expect_ack().await;
    reinvite.expect(488).await;
    scenario_harness::callflow::hangup(&mut alice, &sp.bob).await;
    assert_answered_and_ended(&sp.reboot_and_finish(&calls).await);
}

/// Ringing, then killed: alice CANCELs on the survivor; the CANCEL crosses
/// call 1, the third party and call 2 to bob, and every INVITE ends 487.
#[tokio::test(start_paused = true)]
async fn a_ringing_spiral_survives_a_worker_kill_caller_cancel() {
    ringing_caller_cancel("spiral-takeover-ringing-cancel", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_ringing_spiral_survives_a_worker_kill_caller_cancel_without_record_route() {
    ringing_caller_cancel("spiral-takeover-ringing-cancel-no-rr", RecordRoute::No).await;
}

async fn ringing_caller_cancel(name: &str, record_route: RecordRoute) {
    let mut sp = Spiral::new(name, record_route).await;
    sp.ring().await;
    let calls = sp.replicated_calls().await;
    sp.kill_the_worker().await;

    let mut cancel = sp.call.cancel().await;
    cancel.expect(200).await;
    let mut at_bob = sp.bob.try_receive("CANCEL").await.expect("the CANCEL crosses the spiral");
    at_bob.respond(200, "OK").await;
    sp.uas.respond(487, "Request Terminated").await;
    sp.uas.expect_ack().await;
    sp.call.expect(487).await;
    assert_unanswered(&sp.reboot_and_finish(&calls).await, CdrEventType::Cancel);
}

/// Ringing, then killed: bob rejects 486; the survivor relays it across both
/// calls, each hop ACKing the one below it.
#[tokio::test(start_paused = true)]
async fn a_ringing_spiral_survives_a_worker_kill_callee_reject() {
    ringing_callee_reject("spiral-takeover-ringing-reject", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_ringing_spiral_survives_a_worker_kill_callee_reject_without_record_route() {
    ringing_callee_reject("spiral-takeover-ringing-reject-no-rr", RecordRoute::No).await;
}

async fn ringing_callee_reject(name: &str, record_route: RecordRoute) {
    let mut sp = Spiral::new(name, record_route).await;
    sp.ring().await;
    let calls = sp.replicated_calls().await;
    sp.kill_the_worker().await;

    sp.uas.respond(486, "Busy Here").await;
    sp.uas.expect_ack().await;
    sp.call.expect(486).await;
    assert_unanswered(&sp.reboot_and_finish(&calls).await, CdrEventType::Reject);
}

/// Ringing, then killed: bob answers on the survivor, alice ACKs, bob hangs up.
#[tokio::test(start_paused = true)]
async fn a_ringing_spiral_survives_a_worker_kill_answered_then_ended() {
    ringing_answered_then_ended("spiral-takeover-ringing-answer", RecordRoute::Yes).await;
}

#[tokio::test(start_paused = true)]
async fn a_ringing_spiral_survives_a_worker_kill_answered_then_ended_without_record_route() {
    ringing_answered_then_ended("spiral-takeover-ringing-answer-no-rr", RecordRoute::No).await;
}

async fn ringing_answered_then_ended(name: &str, record_route: RecordRoute) {
    let mut sp = Spiral::new(name, record_route).await;
    sp.ring().await;
    let calls = sp.replicated_calls().await;
    sp.kill_the_worker().await;

    let (_alice, mut bob) = sp.answer().await;
    scenario_harness::callflow::hangup(&mut bob, &sp.alice).await;
    assert_answered_and_ended(&sp.reboot_and_finish(&calls).await);
}
