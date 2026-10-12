//! **A routed call whose node dies while the callee is silent.** The caller
//! heard the node's 100 Trying through the stateless front proxy (RFC 3261
//! §16.11) and runs no Timer B (§17.1.1.2); the callee sends nothing more — no
//! answer, a 100 alone, or a 180 and then nothing. The call's no-answer deadline
//! lives in its replica, so when the node's departure is announced its backup
//! takes the call over at once and ends it at that deadline, as the node would
//! have: the caller gets its 480, the callee its CANCEL (held while its INVITE
//! drew no provisional, §9.1). An established call of the same node, with
//! nothing in flight, is not taken over: it stays reactive (ADR-0014 §1).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use call::{CallBodyCodec, MsgpackCodec, TimerType};
use failover_harness::{FailoverHarness, PartitionRole, ProxySut, ReplicatedB2buaSut};
use scenario_harness::agent::{Agent, ClientInvite, ServerTxn};
use sip_proxy::strategies::rendezvous::{rendezvous_select, RendezvousCandidate};

use crate::departure_support::{bring_up_deciding, kill_announced, CountedDecision};
use crate::inflight_resend::{reboot_and_reclaim, settle_to_one_cdr};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The route's ring deadline.
const NO_ANSWER: Duration = Duration::from_secs(10);
/// How late after its deadline the caller's final may arrive.
const DEADLINE_SLACK_MS: i64 = 1_000;

/// Every call is routed to bob with a ring deadline and no failover.
fn ring_deadline_decision() -> Arc<dyn CallDecisionEngine> {
    let engine = ScriptedDecisionEngine::builder()
        .fallback(|_| {
            let mut r = route_to("127.0.0.1", 5070);
            r.no_answer_ms = Some(NO_ANSWER.as_millis() as i64);
            NewCallResponse::Route(r)
        })
        .build();
    CountedDecision::counting(Arc::new(engine), Duration::ZERO).0
}

struct Worker(&'static str);

impl RendezvousCandidate for Worker {
    fn id(&self) -> &str {
        self.0
    }
}

/// A Call-ID the front proxy places on `ordinal` while both workers are alive.
fn call_id_placed_on(ordinal: &str, label: &str) -> String {
    let workers = [Worker("b1"), Worker("b2")];
    (0..)
        .map(|i| format!("{label}-{i}@127.0.0.1"))
        .find(|cid| rendezvous_select(cid, &workers).is_some_and(|w| w.0 == ordinal))
        .expect("some Call-ID lands on each worker")
}

/// What the callee does once the INVITE reaches it.
#[derive(Clone, Copy)]
enum Callee {
    /// Nothing at all.
    Silent,
    /// A 100 Trying, then nothing.
    TryingOnly,
    /// A 180 Ringing, then nothing.
    Ringing,
}

/// The cluster with one established call and one ringing call, both placed on
/// `b1` and both replicated to `b2`.
struct Placed {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    quiet: scenario_harness::agent::Dialog,
    quiet_ref: String,
    ringing: ClientInvite,
    at_bob: ServerTxn,
    ringing_ref: String,
}

async fn place(name: &str, callee: Callee) -> Placed {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, ring_deadline_decision()).await;

    let quiet_cid = call_id_placed_on("b1", "quiet");
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .identity(quiet_cid, "quiet-tag")
        .through(proxy.addr())
        .send()
        .await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let quiet = call.ack().await;
    bob.receive("ACK").await;

    let ringing_cid = call_id_placed_on("b1", "ringing");
    let mut ringing = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .identity(ringing_cid, "ringing-tag")
        .through(proxy.addr())
        .send()
        .await;
    ringing.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    match callee {
        Callee::Silent => {}
        Callee::TryingOnly => at_bob.respond(100, "Trying").await,
        Callee::Ringing => {
            at_bob.respond(180, "Ringing").await;
            ringing.expect(180).await;
        }
    }
    fh.advance(Duration::from_millis(500)).await;

    let mut refs = w_b2.scan_backed_up("b1");
    assert_eq!(refs.len(), 2, "both calls replicated to b2");
    refs.sort_by_key(|r| !r.contains("ringing"));
    let (ringing_ref, quiet_ref) = (refs[0].clone(), refs[1].clone());
    Placed { fh, alice, bob, proxy, w_b1, w_b2, quiet, quiet_ref, ringing, at_bob, ringing_ref }
}

/// The ring deadline `b2`'s replica of `call_ref` carries.
async fn replicated_deadline(b2: &ReplicatedB2buaSut, call_ref: &str) -> i64 {
    let body = b2.get(PartitionRole::Backup, "b1", call_ref).await.expect("the replica");
    let call = MsgpackCodec::new().decode(&body).expect("the replica decodes");
    call.timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::NoAnswer)
        .map(|t| t.fire_at)
        .expect("the replica carries the ring deadline")
}

/// The departure of `b1` announced: `b2` takes the ringing call over, and the
/// established one stays where it is. Returns the ring deadline.
async fn depart(p: &mut Placed) -> i64 {
    let deadline = replicated_deadline(&p.w_b2, &p.ringing_ref).await;
    let before = kill_announced(&mut p.fh, &mut p.w_b1, &p.w_b2, "b1", &p.proxy).await;
    assert_eq!(
        p.w_b2.metrics().repl_takeover_hydrated_total(),
        before + 1,
        "the departure took exactly one call over",
    );
    assert!(p.w_b2.serves(&p.ringing_ref), "the ringing call is served by the backup");
    assert!(!p.w_b2.serves(&p.quiet_ref), "the established call is not taken over");
    deadline
}

/// Run the clock to just before `deadline`, asserting the caller has no final.
async fn ring_until(p: &Placed, deadline: i64) {
    let wait = deadline - p.fh.now_ms() - 500;
    p.fh.advance(Duration::from_millis(wait.max(0) as u64)).await;
    while let Some(msg) = p.alice.take_queued().await {
        let provisional = matches!(&msg, sip_message::SipMessage::Response(r) if r.status() < 200);
        assert!(provisional, "nothing but a provisional reaches the caller before the deadline");
    }
}

/// The caller's 480, at the deadline.
async fn final_at(p: &mut Placed, deadline: i64) {
    p.ringing.expect(480).await;
    let late = p.fh.now_ms() - deadline;
    assert!(
        (0..=DEADLINE_SLACK_MS).contains(&late),
        "the caller's final follows the replicated deadline, {late} ms after it",
    );
}

/// The established call ends with the caller's BYE, which takes it over
/// reactively; the dead node returns and every call settles to one CDR.
async fn finish(mut p: Placed) {
    let mut bye = p.quiet.bye().await;
    p.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    p.fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut p.fh, &mut p.w_b1, &p.w_b2, "b1", &p.proxy).await;
    for call_ref in [&p.ringing_ref, &p.quiet_ref] {
        settle_to_one_cdr(&p.fh, &[&p.alice, &p.bob], &[&p.w_b1, &p.w_b2], call_ref).await;
    }
    drop(p.proxy);
}

#[tokio::test(start_paused = true)]
async fn a_call_ringing_at_its_nodes_departure_ends_at_its_replicated_deadline() {
    let mut p = place("ringing-call-departure", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(&mut p, deadline).await;
    finish(p).await;
}

/// A 100 is hop-by-hop and leaves no dialog in the record (§16.7), so the
/// backup holds the callee's INVITE as one that drew no provisional: the
/// CANCEL waits for the callee's next provisional (§9.1), and goes out on it.
#[tokio::test(start_paused = true)]
async fn a_call_whose_callee_sent_a_100_alone_ends_at_its_replicated_deadline() {
    let mut p = place("trying-callee-departure", Callee::TryingOnly).await;
    let deadline = depart(&mut p).await;
    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    final_at(&mut p, deadline).await;
    p.fh.advance(Duration::from_secs(2)).await;
    assert!(
        p.bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "the CANCEL waits for a provisional the record holds (§9.1)",
    );
    p.at_bob.respond(180, "Ringing").await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    finish(p).await;
}

#[tokio::test(start_paused = true)]
async fn a_call_whose_callee_is_silent_ends_at_its_replicated_deadline() {
    let mut p = place("silent-callee-departure", Callee::Silent).await;
    let deadline = depart(&mut p).await;
    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    final_at(&mut p, deadline).await;
    // The CANCEL is held (§9.1); the callee's late answer is reaped.
    p.fh.advance(Duration::from_secs(2)).await;
    assert!(
        p.bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "no CANCEL toward a branch that drew no provisional (§9.1)",
    );
    p.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    p.bob.receive_absorbing("ACK", &["INVITE"]).await;
    p.bob.receive_absorbing("BYE", &["INVITE"]).await.respond(200, "OK").await;
    finish(p).await;
}

/// The workers that sent `prefix` toward `to`, by source address.
fn senders(p: &Placed, to: std::net::SocketAddr, prefix: &[u8]) -> std::collections::HashSet<u16> {
    let proxy = p.proxy.addr();
    p.fh.sip_entries()
        .iter()
        .filter(|e| e.to == proxy && e.from != to && e.raw.starts_with(prefix))
        .map(|e| e.from.port())
        .collect()
}

/// The departed node returns before the ringing call's deadline and reclaims
/// it while the backup's copy is still live: neither copy has written since
/// the takeover, so the version vector shows the reclaimed record nothing of
/// the other, and at the deadline both end the call — each node CANCELs the
/// callee and refuses the caller 480 (ADR-0047, residual). The callee's 487
/// to the CANCELled INVITE reaches neither copy and is never ACKed; the call
/// still settles to one CDR.
#[tokio::test(start_paused = true)]
async fn a_ringing_call_whose_node_returns_before_its_deadline_is_ended_by_both_copies() {
    let mut p = place("ringing-call-node-returns", Callee::Ringing).await;
    // What the two copies put on the wire is what this cell measures.
    p.fh.allow_rfc_violation(
        "rung-byte-identical",
        "two live copies send the same CANCEL from two addresses",
    );
    p.fh.allow_rfc_violation(
        "unacked-invite-non-2xx-final",
        "the callee's 487 reaches neither copy's transaction",
    );
    let deadline = depart(&mut p).await;
    let new_addr = p.w_b1.reboot().await;
    p.proxy.set_address("b1", new_addr);
    p.fh.note_worker_rebound("b1", new_addr);
    p.w_b2.simulate_peer_added("b1");
    for _ in 0..40 {
        p.fh.advance(Duration::from_millis(250)).await;
        if p.w_b1.is_ready() {
            break;
        }
    }
    assert!(p.w_b1.is_ready(), "the returning node became ready before the deadline");
    p.proxy.set_health("b1", failover_harness::WorkerHealth::Alive);
    assert!(p.w_b1.serves(&p.ringing_ref), "the returning node serves the call it reclaimed");
    assert!(p.w_b2.serves(&p.ringing_ref), "and the backup's copy is still live");

    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    final_at(&mut p, deadline).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.fh.advance(Duration::from_secs(40)).await;
    p.bob.drain_expecting(Duration::from_millis(100), &["CANCEL"]).await;
    assert_eq!(senders(&p, p.bob.addr(), b"CANCEL").len(), 2, "both copies CANCEL the callee");
    assert_eq!(senders(&p, p.alice.addr(), b"SIP/2.0 480").len(), 2, "both refuse the caller");

    let mut bye = p.quiet.bye().await;
    p.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    p.fh.advance(Duration::from_secs(60)).await;
    for call_ref in [&p.ringing_ref, &p.quiet_ref] {
        settle_to_one_cdr(&p.fh, &[&p.alice, &p.bob], &[&p.w_b1, &p.w_b2], call_ref).await;
    }
    drop(p.proxy);
}
