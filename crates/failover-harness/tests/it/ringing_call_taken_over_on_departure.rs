//! **A routed call whose node dies while the callee is silent.** The caller
//! heard the node's 100 Trying through the stateless front proxy (RFC 3261
//! §16.11) and runs no Timer B (§17.1.1.2); the callee sends nothing more — no
//! answer, a 100 alone, or a 180 and then nothing. The call's no-answer deadline
//! lives in its replica — armed at the callee's 180, or, the record holding
//! none, presumed from its INVITE by the copy that takes it over — so when the
//! node's departure is announced its backup takes the call over at once and
//! ends it at that deadline, as the node would have: the caller gets its 480,
//! the callee its CANCEL (held while its INVITE drew no provisional, §9.1, the
//! INVITE re-sent once so the callee resends its latest one, §17.2.1). An
//! established call of the same node, with nothing in flight, is not taken
//! over: it stays reactive (ADR-0014 §1).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use call::{CallBodyCodec, MsgpackCodec, TimerType};
use failover_harness::{FailoverHarness, PartitionRole, ProxySut, ReplicatedB2buaSut};
use scenario_harness::agent::{Agent, ClientInvite, ServerTxn};

use crate::departure_support::{
    bring_up_deciding, call_id_placed_on, kill_announced, CountedDecision,
};
use crate::inflight_resend::{reboot_and_reclaim, settle_to_one_cdr};

pub(crate) const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
pub(crate) const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The route's ring deadline.
const NO_ANSWER: Duration = Duration::from_secs(10);
/// How late after its deadline the caller's final may arrive.
const DEADLINE_SLACK_MS: i64 = 1_000;

/// Every call is routed to bob with the ring deadline `no_answer` and no
/// failover.
fn ring_deadline_decision(no_answer: Duration) -> Arc<dyn CallDecisionEngine> {
    let engine = ScriptedDecisionEngine::builder()
        .fallback(move |_| {
            let mut r = route_to("127.0.0.1", 5070);
            r.no_answer_ms = Some(no_answer.as_millis() as i64);
            NewCallResponse::Route(r)
        })
        .build();
    CountedDecision::counting(Arc::new(engine), Duration::ZERO).0
}

/// What the callee does once the INVITE reaches it.
#[derive(Clone, Copy)]
pub(crate) enum Callee {
    /// Nothing at all.
    Silent,
    /// A 100 Trying, then nothing.
    TryingOnly,
    /// A 180 Ringing, then nothing.
    Ringing,
}

/// The cluster with one established call and one ringing call, both placed on
/// `b1` and both replicated to `b2`.
pub(crate) struct Placed {
    pub(crate) fh: FailoverHarness,
    pub(crate) alice: Agent,
    pub(crate) bob: Agent,
    pub(crate) proxy: ProxySut,
    pub(crate) w_b1: ReplicatedB2buaSut,
    pub(crate) w_b2: ReplicatedB2buaSut,
    pub(crate) quiet: scenario_harness::agent::Dialog,
    pub(crate) quiet_ref: String,
    pub(crate) ringing: ClientInvite,
    pub(crate) at_bob: ServerTxn,
    pub(crate) ringing_ref: String,
    /// The route's ring deadline.
    pub(crate) no_answer: Duration,
}

pub(crate) async fn place(name: &str, callee: Callee) -> Placed {
    place_ringing_for(name, callee, NO_ANSWER).await
}

/// [`place`] with the route's ring deadline at `no_answer`.
pub(crate) async fn place_ringing_for(name: &str, callee: Callee, no_answer: Duration) -> Placed {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, ring_deadline_decision(no_answer)).await;

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
    Placed {
        fh,
        alice,
        bob,
        proxy,
        w_b1,
        w_b2,
        quiet,
        quiet_ref,
        ringing,
        at_bob,
        ringing_ref,
        no_answer,
    }
}

/// The ring deadline of `b2`'s replica of the ringing call: the one it carries, or,
/// its callee not having rung, the one a takeover presumes from its INVITE.
async fn replicated_deadline(p: &Placed) -> i64 {
    let body = p.w_b2.get(PartitionRole::Backup, "b1", &p.ringing_ref).await.expect("the replica");
    let call = MsgpackCodec::new().decode(&body).expect("the replica decodes");
    let presumed = || {
        let invite = call.b_legs[0].outbound_invite.as_ref().expect("the callee was dialled");
        invite.sent_at_ms + p.no_answer.as_millis() as i64
    };
    call.timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::NoAnswer)
        .map_or_else(presumed, |t| t.fire_at)
}

/// The INVITE the backup re-sends once the CANCEL it holds waits on it (§9.1):
/// the callee's transaction takes it as a retransmission.
async fn resent_invite(p: &Placed) {
    let resent = p.bob.receive("INVITE").await;
    assert_eq!(resent.request().top_via().branch(), p.at_bob.request().top_via().branch());
}

/// The departure of `b1` announced: `b2` takes the ringing call over, and the
/// established one stays where it is. Returns the ring deadline.
pub(crate) async fn depart(p: &mut Placed) -> i64 {
    let deadline = replicated_deadline(p).await;
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
pub(crate) async fn ring_until(p: &Placed, deadline: i64) {
    let wait = deadline - p.fh.now_ms() - 500;
    p.fh.advance(Duration::from_millis(wait.max(0) as u64)).await;
    while let Some(msg) = p.alice.take_queued().await {
        let provisional = matches!(&msg, sip_message::SipMessage::Response(r) if r.status() < 200);
        assert!(provisional, "nothing but a provisional reaches the caller before the deadline");
    }
}

/// The caller's 480, at the deadline.
pub(crate) async fn final_at(p: &mut Placed, deadline: i64) {
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
    // The callee resends its 100: hop-by-hop, it names no worker the proxy
    // could fail it over to, so it never reaches the backup.
    resent_invite(&p).await;
    p.at_bob.respond(100, "Trying").await;
    p.fh.advance(Duration::from_secs(2)).await;
    assert!(
        p.bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
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
    resent_invite(&p).await;
    p.fh.advance(Duration::from_secs(2)).await;
    assert!(
        p.bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no CANCEL toward a branch that drew no provisional (§9.1)",
    );
    p.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    p.bob.receive_absorbing("ACK", &["INVITE"]).await;
    p.bob.receive_absorbing("BYE", &["INVITE"]).await.respond(200, "OK").await;
    finish(p).await;
}
