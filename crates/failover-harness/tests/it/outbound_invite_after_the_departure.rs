//! **A departing worker's outbound INVITE, relayed after the proxy departed
//! it.** A worker accepts a caller's INVITE just before its graceful removal
//! starts (ADR-0031 D2(ii): it still serves what was routed to it). Its routing
//! decision answers after the front proxy has taken it out of its projection
//! and tombstoned its address, so its b-leg INVITE crosses the proxy as the
//! request of a departed worker.
//!
//! ```text
//!   caller INVITE → proxy → P; P 100; the call reaches its backup S
//!   t0        P's endpoint reads not ready + terminating: the proxy departs P,
//!             P latches draining, S keeps pulling it
//!   +0.8 s    P's decision answers: b-leg INVITE → proxy → callee
//!             callee 180
//!   drain     P's drain returns; its endpoint leaves the slice; P exits
//!   +0.5 s    callee 200 (retransmitted until ACKed)
//!   +2 s      (replacement case) P's replacement joins and reclaims the call
//!   INVITE+30 s  the ring deadline the route armed
//! ```
//!
//! The proxy Record-Routes a worker's dialog-creating request with a cookie
//! naming that worker and a backup; it is what routes the callee's responses
//! and in-dialog requests once the worker is gone. The callee's 1xx and 2xx
//! must reach a worker holding the call, the caller takes exactly one final
//! (the 2xx, RFC 3261 §13.2.2.4), no ring deadline authors a second final on
//! her ACKed INVITE server transaction (§17.2.1, §13.3.1.4), the callee is
//! ACKed and takes no CANCEL, and the call settles to one CDR.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, ClientInvite, Dialog, Inbound, ServerTxn};
use sip_message::Method;

use crate::departure_support::{bring_up_deciding, call_id_placed_on, placed, CountedDecision};
use crate::withdrawn_primary_answers_in_its_window::{
    hops, no_answer_decision, report_dir, BOUNDS, NO_ANSWER_SEC,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long the routing decision takes: the withdrawal lands inside it.
const DECISION_DELAY: Duration = Duration::from_millis(1_000);
/// From the caller's INVITE to the withdrawal.
const WITHDRAW_AFTER_INVITE: Duration = Duration::from_millis(200);
/// From the elder's exit to the callee's answer.
const ANSWER_AFTER_EXIT: Duration = Duration::from_millis(500);
/// From the callee's answer to the replacement's start.
const REPLACEMENT_AFTER_ANSWER: Duration = Duration::from_secs(2);
/// The clock step the parties read their sockets at.
const STEP: Duration = Duration::from_millis(100);
/// RFC 3261 timer values (§17.1.1.1, Table 4).
const T1_MS: i64 = 500;
const T2_MS: i64 = 4_000;

/// Whether a replacement of the departed ordinal ever joins.
#[derive(Clone, Copy, PartialEq)]
enum Replacement {
    /// A rolling update: the same ordinal comes back at a new address.
    Joins,
    /// A scale-down: the ordinal never comes back.
    Never,
}

/// What the two parties took, answered as conformant UAs.
#[derive(Default)]
struct Parties {
    /// Every final the caller's INVITE client transaction took, in order.
    finals_to_alice: Vec<u16>,
    /// The caller's dialog, from the first 2xx.
    dialog: Option<Dialog>,
    /// ACKs the callee drew for its 2xx.
    acks_to_bob: usize,
    /// CANCELs the callee took.
    cancels_to_bob: usize,
    /// Copies of the answered INVITE the callee took after its 2xx.
    invite_resends_to_bob: usize,
    /// BYEs the caller took from the B2BUA.
    byes_to_alice: usize,
    /// BYEs the callee took.
    byes_to_bob: usize,
    /// The final the caller's BYE took.
    bye_final: Option<u16>,
}

/// The callee's answered INVITE server transaction: its TU retransmits the 2xx
/// at T1, doubling up to T2, until the ACK arrives or 64·T1 has passed
/// (RFC 3261 §13.3.1.4).
struct CalleeAnswer {
    uas: ServerTxn,
    answered_at: i64,
    next_at: i64,
    interval_ms: i64,
}

impl CalleeAnswer {
    async fn answer(uas: ServerTxn, now: i64) -> Self {
        let mut a = Self { uas, answered_at: now, next_at: now + T1_MS, interval_ms: T1_MS };
        a.uas.respond(200, "OK").with_sdp(ANSWER).await;
        a
    }

    /// Retransmit the 2xx when its turn is due and no ACK has arrived.
    async fn tick(&mut self, now: i64, acked: bool) {
        if acked || now >= self.answered_at + 64 * T1_MS || now < self.next_at {
            return;
        }
        self.uas.respond(200, "OK").with_sdp(ANSWER).await;
        self.interval_ms = (self.interval_ms * 2).min(T2_MS);
        self.next_at = now + self.interval_ms;
    }
}

/// Pump until `until_ms`, answering both parties as conformant UAs: the
/// caller ACKs a 2xx on the dialog it establishes (RFC 3261 §13.2.2.4) and
/// hop-ACKs a non-2xx final (§17.1.1.3); the callee retransmits its 2xx until
/// ACKed (§13.3.1.4); a CANCEL reaching the callee after its 2xx matches no
/// transaction and draws a 481 (§9.2); a BYE or a keepalive OPTIONS draws a
/// 200.
async fn pump_parties(
    fh: &FailoverHarness,
    call: &ClientInvite,
    alice: &Agent,
    bob: &Agent,
    until_ms: i64,
    seen: &mut Parties,
    answer: &mut Option<CalleeAnswer>,
) {
    while fh.now_ms() < until_ms {
        fh.advance(STEP).await;
        if let Some(a) = answer.as_mut() {
            a.tick(fh.now_ms(), seen.acks_to_bob > 0).await;
        }
        while let Some(msg) = alice.take_queued_inbound().await {
            match msg {
                Inbound::Response(r) if *r.cseq().method() == Method::Invite => {
                    if r.status() < 200 {
                        continue;
                    }
                    seen.finals_to_alice.push(r.status());
                    if r.status() >= 300 {
                        call.ack_non_2xx(&r).await.expect("the caller hop-ACKs the final");
                    } else {
                        let mut dialog = call.fork_dialog(&r);
                        dialog.ack(None).await;
                        if seen.dialog.is_none() {
                            seen.dialog = Some(dialog);
                        }
                    }
                }
                Inbound::Response(r) if *r.cseq().method() == Method::Bye => {
                    seen.bye_final = Some(r.status());
                }
                Inbound::Response(_) => {}
                Inbound::Request(mut t) => {
                    if t.request().method() == "BYE" {
                        seen.byes_to_alice += 1;
                    }
                    t.respond(200, "OK").await;
                }
            }
        }
        while let Some(msg) = bob.take_queued_inbound().await {
            match msg {
                Inbound::Request(t) if t.request().method() == "ACK" => seen.acks_to_bob += 1,
                Inbound::Request(t)
                    if t.request().method() == "INVITE" && t.request().to().tag().is_none() =>
                {
                    // The answered INVITE again, past its server transaction:
                    // the callee's core answers it with its 2xx (§13.3.1.4).
                    seen.invite_resends_to_bob += 1;
                    if let Some(a) = answer.as_mut() {
                        a.uas.respond(200, "OK").with_sdp(ANSWER).await;
                    }
                }
                Inbound::Request(mut t) if t.request().method() == "CANCEL" => {
                    seen.cancels_to_bob += 1;
                    t.respond(481, "Call/Transaction Does Not Exist").await;
                }
                Inbound::Request(mut t) => {
                    if t.request().method() == "BYE" {
                        seen.byes_to_bob += 1;
                    }
                    t.respond(200, "OK").await;
                }
                Inbound::Response(_) => {}
            }
        }
    }
}

/// Whether the proxy relayed a callee response starting with `prefix` to a
/// worker (any address but the two parties'), delivered.
fn relayed_to_a_worker(fh: &FailoverHarness, prefix: &str) -> bool {
    let (alice, proxy): (SocketAddr, SocketAddr) =
        (ALICE.parse().unwrap(), "127.0.0.1:5080".parse().unwrap());
    hops(fh, prefix, "INVITE")
        .iter()
        .any(|(from, to, delivered)| *from == proxy && *to != alice && *delivered)
}

async fn run(name: &str, title: &str, replacement: Replacement) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (decision, _consults) = CountedDecision::counting(no_answer_decision(), DECISION_DELAY);
    let (proxy, mut w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;

    // ── the caller's INVITE reaches b1, which asks for its route ────────────
    let call_id = call_id_placed_on("b1", "departing-outbound");
    let invite_at = fh.now_ms();
    let call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .identity(call_id, "alice-tag-1")
        .through(proxy.addr())
        .send()
        .await;
    let (pri, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    assert_eq!(pri, "b1", "the proxy placed the call on b1");
    let fire_at = invite_at + NO_ANSWER_SEC * 1000;

    // ── t0: the graceful removal starts while the decision is pending ───────
    let wait = (invite_at + WITHDRAW_AFTER_INVITE.as_millis() as i64 - fh.now_ms()).max(0);
    fh.advance(Duration::from_millis(wait as u64)).await;
    fh.withdraw_routing("b1");
    assert!(proxy.health("b1").is_none(), "the proxy departed b1");
    assert_eq!(
        proxy.health_at(w_b1.sip_addr()),
        Some(WorkerHealth::Dead),
        "b1's address is tombstoned"
    );
    let mut drain = fh.begin_drain_pending(&w_b1, BOUNDS);

    // ── the decision answers: the b-leg INVITE crosses the proxy ────────────
    let bob_addr: SocketAddr = BOB.parse().unwrap();
    let relayed = fh
        .pump_until(STEP, Duration::from_secs(3), async || {
            hops(&fh, "INVITE ", "INVITE").iter().any(|(_, to, d)| *d && *to == bob_addr)
        })
        .await;
    assert!(relayed, "b1's decision answered and its b-leg INVITE reached the callee");
    let mut uas = bob.receive("INVITE").await;
    let (w_pri, w_bak) = worker_ordinals(uas.request());
    fh.mark(
        "b1",
        None,
        "b-leg INVITE",
        &format!(
            "relayed after the departure; Record-Route cookie w_pri={w_pri:?} w_bak={w_bak:?}"
        ),
    );
    uas.respond(180, "Ringing").await;
    fh.advance(Duration::from_millis(300)).await;
    drain.poll();
    let ringing_relayed = relayed_to_a_worker(&fh, "SIP/2.0 180");
    fh.mark("b1", None, "callee 180", &format!("relayed to a worker: {ringing_relayed}"));

    // ── the drain returns; the endpoint leaves the slice; the process exits ─
    let mut outcome = None;
    for _ in 0..120 {
        if let Some(o) = drain.poll() {
            outcome = Some(o);
            break;
        }
        fh.advance(STEP).await;
    }
    drop(drain);
    let outcome = outcome.expect("the drain returned inside its grace");
    fh.mark(
        "b1",
        None,
        "exit",
        &format!("the drain returned {:?} with {} live call(s)", outcome.exit, outcome.residual),
    );
    fh.depart("b1");
    w_b1.crash();

    // ── the callee answers ───────────────────────────────────────────────────
    fh.advance(ANSWER_AFTER_EXIT).await;
    fh.mark("b1", None, "answer", "the callee answers 200");
    let mut answer = Some(CalleeAnswer::answer(uas, fh.now_ms()).await);
    let mut seen = Parties::default();
    pump_parties(&fh, &call, &alice, &bob, fh.now_ms() + 300, &mut seen, &mut answer).await;
    let answer_relayed = relayed_to_a_worker(&fh, "SIP/2.0 200");
    fh.mark("b1", None, "callee 200", &format!("relayed to a worker: {answer_relayed}"));

    // ── a replacement of the same ordinal joins (rolling update) ────────────
    let mut reborn: Option<ReplicatedB2buaSut> = None;
    if replacement == Replacement::Joins {
        pump_parties(
            &fh,
            &call,
            &alice,
            &bob,
            fh.now_ms() + REPLACEMENT_AFTER_ANSWER.as_millis() as i64,
            &mut seen,
            &mut answer,
        )
        .await;
        let r = fh.spawn_replacement("b1").await;
        let ready = fh.pump_until(STEP, Duration::from_secs(30), async || r.is_ready()).await;
        assert!(ready, "the replacement bootstrapped");
        fh.readmit("b1", r.sip_addr());
        proxy.set_health("b1", WorkerHealth::Alive);
        fh.mark(
            "b1",
            None,
            "replacement alive",
            &format!(
                "replacement({}) serves={} survivor serves={}",
                r.sip_addr(),
                r.serves(&call_ref),
                w_b2.serves(&call_ref)
            ),
        );
        reborn = Some(r);
    }

    // ── across the ring deadline ─────────────────────────────────────────────
    pump_parties(&fh, &call, &alice, &bob, fire_at + 3_000, &mut seen, &mut answer).await;
    fh.mark(
        "b1",
        None,
        "past the ring deadline",
        &format!(
            "caller finals={:?} callee ACKs={} CANCELs={} INVITE resends={} BYEs to caller={} \
             to callee={}",
            seen.finals_to_alice,
            seen.acks_to_bob,
            seen.cancels_to_bob,
            seen.invite_resends_to_bob,
            seen.byes_to_alice,
            seen.byes_to_bob
        ),
    );

    // ── the caller hangs up an established call ──────────────────────────────
    if let Some(dialog) = seen.dialog.as_mut() {
        if seen.byes_to_alice == 0 {
            let _bye = dialog.bye().await;
            pump_parties(&fh, &call, &alice, &bob, fh.now_ms() + 5_000, &mut seen, &mut answer)
                .await;
        }
    }
    let mut nodes: Vec<&ReplicatedB2buaSut> = vec![&w_b1, &w_b2];
    if let Some(r) = reborn.as_ref() {
        nodes.push(r);
    }
    let released = fh
        .settle_terminal(async || {
            let mut clean = true;
            for n in &nodes {
                if n.holds_any_trace(&call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let cdrs = total_cdrs_for(&nodes, &call_ref);
    let mut holders = Vec::new();
    for n in &nodes {
        if n.holds_any_trace(&call_ref).await {
            holders.push(format!("{}(serves={})", n.incarnation_key(), n.serves(&call_ref)));
        }
    }

    let cookie_ok = w_pri == "b1" && w_bak == "b2";
    let passed = cookie_ok
        && ringing_relayed
        && answer_relayed
        && seen.finals_to_alice == [200]
        && seen.acks_to_bob >= 1
        && seen.cancels_to_bob == 0
        && seen.byes_to_alice == 0
        && seen.bye_final == Some(200)
        && released
        && cdrs == 1;
    fh.mark(
        "b1",
        None,
        "outcome",
        &format!(
            "cookie={cookie_ok} 180 relayed={ringing_relayed} 200 relayed={answer_relayed} \
             caller finals={:?} callee ACKs={} caller BYE final={:?} released={released} \
             still held by {holders:?} CDRs={cdrs}",
            seen.finals_to_alice, seen.acks_to_bob, seen.bye_final,
        ),
    );
    let written = fh
        .write_unified_report(&report_dir(name), "report", title, passed)
        .expect("report written");
    eprintln!("report: {}", written[0].display());

    assert!(
        cookie_ok,
        "the b-leg INVITE b1 sent after its departure is Record-Routed with a cookie naming \
         b1 and its alive backup b2: got w_pri={w_pri:?} w_bak={w_bak:?}"
    );
    assert!(ringing_relayed, "the callee's 180 reached a worker");
    assert!(answer_relayed, "the callee's 200 reached a worker");
    assert_eq!(
        seen.finals_to_alice,
        [200],
        "the caller takes exactly one final, the 2xx, and no final after it \
         (RFC 3261 §13.2.2.4, §17.2.1)"
    );
    assert!(seen.acks_to_bob >= 1, "the callee's 2xx is ACKed");
    assert_eq!(seen.cancels_to_bob, 0, "the callee's answered INVITE takes no CANCEL");
    assert_eq!(seen.byes_to_alice, 0, "the B2BUA does not end the answered call");
    assert_eq!(seen.bye_final, Some(200), "the caller's BYE ends the call");
    assert!(released, "every node released the call");
    assert_call_fully_released(&nodes, &call_ref).await;
    assert_eq!(cdrs, 1, "exactly one CDR across the cluster");
    fh.assert_sip_rfc_clean(name);
}

/// **Rolling update.** The departed ordinal comes back at a new address after
/// the callee answered.
#[tokio::test(start_paused = true)]
async fn a_b_leg_invite_relayed_after_its_worker_departed_is_answered_once_across_a_replacement() {
    run(
        "outbound-invite-after-the-departure-replaced",
        "A departing worker's b-leg INVITE relayed after the departure (replacement joins)",
        Replacement::Joins,
    )
    .await;
}

/// **Scale-down.** No replacement ever joins: only the survivor can carry
/// the call.
#[tokio::test(start_paused = true)]
async fn a_b_leg_invite_relayed_after_its_worker_departed_is_answered_once_with_no_replacement() {
    run(
        "outbound-invite-after-the-departure-scaled-down",
        "A departing worker's b-leg INVITE relayed after the departure (no replacement)",
        Replacement::Never,
    )
    .await;
}
