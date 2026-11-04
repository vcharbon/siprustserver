//! **A dead verdict that meets the backup's replication stream down.** The
//! backup confirms a proxy's dead verdict by itself (ADR-0047 D1): when its
//! stream from the primary is down or reconnecting at the verdict, it makes
//! one connect at once, skipping the backoff, bounded by the connect bound. A
//! frame heard on it speaks for a live primary, and nothing is taken over; a
//! connect refused or unanswered confirms the verdict.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ primary ─INVITE─▶ bob, 180 back; the call rings
//!   the backup's stream from the primary drops   (backoff | connect | cut | standby proxy)
//!   a proxy reads the live primary dead ─▶ backup: one connect at once ─▶ a frame: nothing taken over
//!   the proxy reads it alive; bob's 200 reaches the primary; ACK, BYE, one CDR
//! ```
//!
//! The twin cells take over a primary that is really gone: one whose host
//! answers no connect is confirmed at the connect bound. A crashed primary
//! refuses the connect and is confirmed at once
//! (`read_dead_primary_taken_over.rs`).
//!
//! Beyond the failover sweep: a misread primary, which no sweep fault makes.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::{FailoverHarness, WorkerHealth};

use crate::departure_support::{bring_up_deciding, placed, CountedDecision, B1, B2};
use crate::inflight_resend::{settle_to_one_cdr, split_mut, survivor_of, REPLICATE};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// A second front proxy, standing by: the call never routes through it.
const STANDBY: &str = "127.0.0.1:5081";

/// How long the routing decision takes to answer.
const DECISION_DELAY: Duration = Duration::from_millis(300);
/// The pullers' idle bound (ADR-0045): a stream that carries nothing this
/// long is left.
const IDLE_BOUND: Duration = Duration::from_secs(2);
/// The pullers' connect bound (ADR-0045).
const CONNECT_BOUND: Duration = Duration::from_secs(2);
/// How soon after its confirmation a verdict dials the callee.
const TAKEOVER_BOUND: Duration = Duration::from_millis(500);
/// Long enough for both ends to read a cut stream ended, and shorter than the
/// pullers' first backoff (100 ms): the verdict meets the puller backing off.
const CUT_SEEN: Duration = Duration::from_millis(10);
/// How far into its own connect the puller is at the verdict: the stream is
/// left at most the idle bound after the stall and connects again 100 ms
/// later, so this connect runs out between 0.1 s and 0.6 s after the verdict.
const IN_FLIGHT: Duration = Duration::from_millis(1_500);
/// When the address answers again after the verdict: after the connect in
/// flight ran out, within the bound of one started at the verdict.
const ANSWERS_AFTER: Duration = Duration::from_millis(800);
/// A partition long enough for the backup's puller to back off for more than
/// a second between two refused connects.
const LONG_PARTITION: Duration = Duration::from_secs(6);

/// What the backup's stream from the live primary is doing when the verdict
/// arrives.
#[derive(Clone, Copy, Debug)]
enum Down {
    /// Its puller backs off between refused connects; the path is back.
    Backoff,
    /// Its puller's connect waits on an address that answers nothing; the
    /// address answers once that connect ran out, within the bound of one
    /// started at the verdict.
    Connecting,
    /// The stream was just cut; the path is back.
    Cut,
    /// The stream was just cut, and the proxy that reads the primary dead is
    /// a standby the call never routes through.
    CutStandbyReads,
}

/// A ringing call whose live primary a proxy reads dead while the backup's
/// stream from it is down: nothing is taken over, and the call completes end
/// to end through its primary with one CDR.
async fn a_misread_takes_nothing_over(name: &str, down: Down) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;
    let workers = [("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())];
    let standby = match down {
        Down::CutStandbyReads => Some(fh.spawn_proxy(STANDBY, &workers).await),
        _ => None,
    };
    let reader = standby.as_ref().unwrap_or(&proxy);

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(REPLICATE).await;
    let backup = survivor_of(&pri_ord, &w_b1, &w_b2);
    let hydrated = backup.metrics().repl_takeover_hydrated_total();

    match down {
        Down::Backoff => {
            fh.partition(&pri_ord, bak_ord);
            fh.advance(LONG_PARTITION).await;
            fh.heal(&pri_ord, bak_ord);
        }
        Down::Connecting => {
            fh.stall(&pri_ord, bak_ord);
            fh.black_hole(&pri_ord);
            fh.advance(IDLE_BOUND + IN_FLIGHT).await;
        }
        Down::Cut | Down::CutStandbyReads => {
            fh.cut_streams(&pri_ord, bak_ord);
            fh.advance(CUT_SEEN).await;
            fh.heal(&pri_ord, bak_ord);
        }
    }
    fh.mark("proxy", None, "misread", "a proxy reads the live primary dead");
    reader.set_health(&pri_ord, WorkerHealth::Dead);
    if matches!(down, Down::Connecting) {
        fh.advance(ANSWERS_AFTER).await;
        fh.heal(&pri_ord, bak_ord);
    }
    fh.advance(CONNECT_BOUND + IDLE_BOUND).await;
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");
    assert_eq!(
        backup.metrics().dead_verdicts_unconfirmed_total(),
        1,
        "the one connect heard the primary"
    );
    assert_eq!(
        backup.metrics().repl_takeover_hydrated_total(),
        hydrated,
        "the verdict takes nothing over"
    );
    assert!(!backup.serves(&call_ref), "the ringing call stays with its primary");

    fh.mark("proxy", None, "flip", "the proxy reads the primary alive again");
    reader.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(standby);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_verdict_during_the_pullers_backoff_takes_nothing_over() {
    a_misread_takes_nothing_over("verdict-during-backoff", Down::Backoff).await;
}

#[tokio::test(start_paused = true)]
async fn a_verdict_during_the_pullers_connect_takes_nothing_over() {
    a_misread_takes_nothing_over("verdict-during-connect", Down::Connecting).await;
}

#[tokio::test(start_paused = true)]
async fn a_verdict_after_a_stream_drop_takes_nothing_over() {
    a_misread_takes_nothing_over("verdict-after-a-stream-drop", Down::Cut).await;
}

#[tokio::test(start_paused = true)]
async fn a_standby_proxys_verdict_at_a_stream_drop_takes_nothing_over() {
    a_misread_takes_nothing_over("standby-verdict-at-a-stream-drop", Down::CutStandbyReads).await;
}

/// **A host that answers nothing.** The primary's host goes silent on every
/// plane: its stream carries nothing and is left at the idle bound, and a
/// connect to it gets no answer. The proxy reads it dead; the backup's one
/// connect runs out at the connect bound, which confirms the verdict, and the
/// backup dials the callee from its record.
#[tokio::test(start_paused = true)]
async fn a_host_answering_no_connect_is_confirmed_at_the_connect_bound_and_taken_over() {
    let name = "unanswering-host-taken-over";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    fh.cut_signalling("bob", bob.addr());
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(DECISION_DELAY + REPLICATE).await;

    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };
    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let primary_sip = primary.sip_addr();
    fh.stall(&pri_ord, bak_ord);
    fh.stall(bak_ord, &pri_ord);
    fh.black_hole(&pri_ord);
    fh.cut_signalling(&pri_ord, primary_sip);
    fh.restore_signalling("bob", bob.addr());
    fh.advance(IDLE_BOUND + Duration::from_millis(500)).await;

    fh.mark(&pri_ord, None, "health", "the proxy's probe reads it dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    let verdict_at = fh.now_ms();
    let mut uas = bob.receive("INVITE").await;
    let dialled_after = fh.now_ms() - verdict_at;
    assert!(
        dialled_after >= CONNECT_BOUND.as_millis() as i64,
        "the takeover waits for the one connect ({dialled_after} ms)",
    );
    assert!(
        dialled_after <= (CONNECT_BOUND + TAKEOVER_BOUND).as_millis() as i64,
        "the callee is dialled once the connect runs out ({dialled_after} ms)",
    );
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.metrics().dead_verdicts_unconfirmed_total(), 0, "the verdict confirmed");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    primary.crash();
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    survivor.simulate_peer_removed(&pri_ord);
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
