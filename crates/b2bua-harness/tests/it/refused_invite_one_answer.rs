//! A new INVITE refused before any transaction exists draws one answer,
//! whichever stage judges each of its copies: the ingress brake on arrival or
//! the transaction layer's deferred-backlog ceiling behind the queue. Every
//! copy gets the same bytes — one To-tag, one `Retry-After`, no `Reason` — as
//! a stateless UAS owes a retransmission (RFC 3261 §8.2.7), and the INVITE is
//! counted once on `b2bua_new_calls_total`. A copy of an INVITE a live server
//! transaction holds is that transaction's to answer, never the brake's
//! (RFC 3261 §17.2.1): one INVITE, one final.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::ingress_brake::IngressBrakeConfig;
use b2bua::new_calls::{NewCallCounts, Refusal};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, B2buaSutBuilder, OFFER_SDP};
use scenario_harness::Agent;
use sip_message::{CustomParser, Method, SipMessage, SipParser};

const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A SUT whose brake engages at `threshold_pct` of `queue_max`, refusing
/// under a jittered `Retry-After`, so two independently phrased answers to one
/// INVITE would differ.
fn braked(bob_port: u16, queue_max: usize, threshold_pct: u32) -> B2buaSutBuilder {
    B2buaSut::route_all_to("127.0.0.1", bob_port)
        .ingress_brake(IngressBrakeConfig { queue_max, threshold_pct })
        .tune(|c| {
            c.retry_after_base_sec = 5;
            c.retry_after_jitter_sec = 30;
        })
}

/// Every final to an INVITE `agent` received, as it came off the wire.
async fn invite_finals(agent: &Agent) -> Vec<(u16, Vec<u8>)> {
    agent.sight_queued().await;
    agent
        .wire_view()
        .into_iter()
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r))
                if r.status() >= 200 && *r.cseq().method() == Method::Invite =>
            {
                Some((r.status(), e.raw))
            }
            _ => None,
        })
        .collect()
}

/// The statuses of every response to an INVITE `agent` received.
async fn invite_statuses(agent: &Agent) -> Vec<u16> {
    agent.sight_queued().await;
    agent
        .wire_view()
        .into_iter()
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r)) if *r.cseq().method() == Method::Invite => Some(r.status()),
            _ => None,
        })
        .collect()
}

/// Wait until `agent` has received `n` responses to an INVITE (finals only
/// when `finals`), or about a second has passed.
async fn received(agent: &Agent, n: usize, finals: bool) {
    for _ in 0..200 {
        let got = if finals {
            invite_finals(agent).await.len()
        } else {
            invite_statuses(agent).await.len()
        };
        if got >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The two 503s `agent` received are one answer: the same bytes, a To-tag, a
/// `Retry-After`, no `Reason`.
async fn assert_one_answer(agent: &Agent) {
    let finals = invite_finals(agent).await;
    assert_eq!(finals.len(), 2, "one 503 per copy: {finals:?}");
    let text: Vec<String> =
        finals.iter().map(|(_, raw)| String::from_utf8_lossy(raw).into_owned()).collect();
    assert!(finals.iter().all(|(status, _)| *status == 503), "{text:?}");
    assert_eq!(text[0], text[1], "every copy of one INVITE draws the same bytes (RFC 3261 §8.2.7)");
    let to = text[0].lines().find(|l| l.starts_with("To:")).expect("a To line");
    assert!(to.contains(";tag="), "{to}");
    assert!(text[0].contains("\r\nRetry-After: "), "{}", text[0]);
    assert!(!text[0].contains("\r\nReason:"), "a refused INVITE's 503 carries no Reason");
}

/// Every series but `rejected` is 0, and the INVITE is counted once.
fn assert_counted_once(counts: &NewCallCounts, rejected: Refusal) {
    assert_eq!(counts.rejected(rejected, Class::Normal), 1, "{}", rejected.as_str());
    assert_eq!(counts.total(), 1, "one count for one INVITE");
}

/// Every call ended, every resource released, the audit gated.
async fn finish(s: B2buaScene) {
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    s.finish().await;
}

/// The transaction layer refuses the first copy at its backlog ceiling; the
/// retransmission meets the brake above its threshold, which answers it with
/// the same 503. One refusal, counted at the backlog.
#[tokio::test]
async fn a_copy_refused_at_the_backlog_draws_the_same_answer_at_the_brake() {
    let s = B2buaScene::with_b2bua("refused-invite-backlog-then-brake", |bob_port| {
        braked(bob_port, 256, 50).deferred_backlog_ceilings(0, 0)
    })
    .await;
    let brake = s.b2bua.ingress_brake().expect("the brake is installed").clone();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    settle_until(|| s.b2bua.txn_metrics().deferred_refused(Class::Normal) == 1).await;
    s.b2bua.force_brake_depth(Some(200));
    call.retransmit().await;
    settle_until(|| brake.refused_copies() == 1).await;
    assert_eq!(brake.refused(Class::Normal), 0, "the brake answered the copy alone");
    s.b2bua.force_brake_depth(None);
    call.expect(503).await;
    received(&s.alice, 2, true).await;

    assert_one_answer(&s.alice).await;
    assert_counted_once(&s.b2bua.new_calls(), Refusal::DeferredBacklog);
    finish(s).await;
}

/// Two copies arrive together: the first finds the queue empty and is queued,
/// the second finds it at the brake's threshold and is shed. The queued copy
/// then reaches the transaction layer, which answers it with the brake's 503.
/// One refusal, counted at the brake.
#[tokio::test(start_paused = true)]
async fn a_copy_shed_at_the_brake_draws_the_same_answer_from_the_transaction_layer() {
    // A threshold of one queued datagram.
    let s = B2buaScene::with_b2bua("refused-invite-brake-then-backlog", |bob_port| {
        braked(bob_port, 100, 1).deferred_backlog_ceilings(0, 0)
    })
    .await;
    let brake = s.b2bua.ingress_brake().expect("the brake is installed").clone();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    call.retransmit().await;
    call.expect(503).await;
    received(&s.alice, 2, true).await;
    assert_eq!(brake.refused(Class::Normal), 1, "the brake shed one copy, the other was queued");
    assert_eq!(s.b2bua.new_calls().refused_copies(), 1, "the queued copy is answered as a copy");

    assert_one_answer(&s.alice).await;
    assert_counted_once(&s.b2bua.new_calls(), Refusal::IngressBrake);
    finish(s).await;
}

/// The same two copies with no backlog ceiling: the queued copy reaches a
/// transaction layer that would admit it, but its INVITE is already refused,
/// so it draws the brake's 503 and no call is born behind the caller's back.
#[tokio::test(start_paused = true)]
async fn a_queued_copy_of_a_shed_invite_is_never_admitted() {
    let s =
        B2buaScene::with_b2bua("refused-invite-queued-copy", |bob_port| braked(bob_port, 100, 1))
            .await;
    let brake = s.b2bua.ingress_brake().expect("the brake is installed").clone();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    call.retransmit().await;
    call.expect(503).await;
    received(&s.alice, 2, true).await;
    assert_eq!(brake.refused(Class::Normal), 1, "the brake shed one copy, the other was queued");
    assert_eq!(s.b2bua.new_calls().refused_copies(), 1, "the queued copy is answered as a copy");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        s.b2bua.new_calls().accepted(Class::Normal),
        0,
        "the refused INVITE is never admitted"
    );
    assert_eq!(s.b2bua.active_calls(), 0);

    assert_one_answer(&s.alice).await;
    assert_counted_once(&s.b2bua.new_calls(), Refusal::IngressBrake);
    finish(s).await;
}

/// An INVITE admitted below the threshold, its retransmission arriving once
/// the brake is above it — sent before the 100 Trying reached the caller
/// (RFC 3261 §17.1.1.2): the live server transaction absorbs the copy and
/// re-sends its 100; the brake sends nothing. The call is set up and counted
/// once, accepted.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_admitted_invite_is_spared_by_the_brake() {
    let s = B2buaScene::with_b2bua("refused-invite-admitted-copy", |bob_port| {
        braked(bob_port, 256, 50)
    })
    .await;
    let brake = s.b2bua.ingress_brake().expect("the brake is installed").clone();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    // The INVITE lands one transit hop later and opens its transaction; the
    // 100 Trying is then one hop away from the caller.
    settle_until(|| s.b2bua.txn_metrics().active_transactions() > 0).await;
    s.alice.sight_queued().await;
    assert!(s.alice.wire_view().is_empty(), "the 100 Trying has not reached the caller yet");
    s.b2bua.force_brake_depth(Some(200));
    call.retransmit().await;
    received(&s.alice, 2, false).await;
    s.b2bua.force_brake_depth(None);
    assert_eq!(invite_statuses(&s.alice).await, [100, 100], "the transaction answers the copy");
    assert_eq!(brake.counts(), Default::default(), "the brake sent nothing");

    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;

    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 1);
    assert_eq!(counts.total(), 1, "one count for one INVITE");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1);
    finish(s).await;
}
