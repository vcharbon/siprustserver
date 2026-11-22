//! A 2xx crosses the caller's CANCEL on a network that loses datagrams. The
//! callee answers as the caller's ringing reaches her; the B2BUA relays the
//! 2xx and the network loses its first copy. Her CANCEL reaches the B2BUA
//! after it, and its 200 to the CANCEL reaches her first (RFC 3261 §9.2);
//! the 2xx goes again on its retransmission timer (§13.3.1.4) and she ACKs
//! it and hangs up. The B2BUA answered before it settled the CANCEL, so the
//! call is answered despite the CANCEL. On a run that declares no loss model
//! the judge stays strict: the same 2xx held back past the CANCEL's 200 with
//! nothing lost, or lost with no loss noted, arrives after the CANCEL was
//! settled: no accepted ending.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{B2buaScene, B2buaSut};
use scenario_harness::actor::scenarios::AbandonRinging;
use scenario_harness::actor::{run_actor_scenario, ScenarioVerdict};
use scenario_harness::realcall::{CallCtx, CallEnv};
use scenario_harness::{Harness, StepError, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::sniff;
use sip_net_testkit::{Fate, SendDelay, SendFate, SimulatedSignalingNetwork};

/// Whether `raw`, sent by `src`, is the B2BUA's 2xx to the caller's INVITE,
/// the first time `first` is asked of one.
fn first_relayed_answer(src: SocketAddr, raw: &[u8], first: &AtomicBool) -> bool {
    let b2bua: SocketAddr = format!("127.0.0.1:{}", b2bua_harness::B2BUA_PORT).parse().unwrap();
    let id = sniff::identity(raw);
    src == b2bua
        && id.status.is_some_and(|s| (200..300).contains(&s))
        && id.cseq_method == Some("INVITE")
        && !first.swap(true, Ordering::SeqCst)
}

/// The fabric loses the B2BUA's first 2xx to the caller; every other
/// datagram arrives.
fn lose_first_relayed_answer() -> SendFate {
    let first = AtomicBool::new(false);
    Arc::new(
        move |src, _dst, raw| {
            if first_relayed_answer(src, raw, &first) {
                Fate::Lose
            } else {
                Fate::Deliver
            }
        },
    )
}

/// The fabric holds the B2BUA's first 2xx to the caller back 250 ms: it
/// arrives once, after the 200 to her CANCEL, and her ACK reaches the B2BUA
/// before the 2xx goes again.
fn hold_first_relayed_answer() -> SendDelay {
    let first = AtomicBool::new(false);
    Arc::new(move |src, _dst, raw| {
        first_relayed_answer(src, raw, &first).then_some(Duration::from_millis(250))
    })
}

/// The caller CANCELs once she rang; the callee answers one hop after he
/// rang, so his 2xx reaches the B2BUA before her CANCEL does.
async fn answer_crossing_the_callers_cancel(h: Harness) -> ScenarioVerdict {
    let s =
        B2buaScene::on_harness(h, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;
    let mut env =
        CallEnv::for_functional(&s.alice, &s.bob, None, s.b2bua.addr, "X-Loadgen-Id", "crossing");
    env.ring_delay = Duration::from_millis(SIMULATED_TRANSIT_DELAY_MS);
    let ctx = CallCtx::new();
    s.h.note_losses(ctx.loss_notes(), &s.alice, &[&s.bob]);
    let verdict = run_actor_scenario(&AbandonRinging, &env, &ctx).await;
    s.finish().await;
    verdict
}

#[tokio::test(start_paused = true)]
async fn a_2xx_sent_before_the_cancel_final_and_lost_first_is_answered_despite_cancel() {
    let h = Harness::with_transit_delay_and_send_fate(
        "answer-across-a-lost-copy",
        SIMULATED_TRANSIT_DELAY_MS,
        lose_first_relayed_answer(),
    );
    let verdict = answer_crossing_the_callers_cancel(h).await;
    assert!(
        matches!(verdict, ScenarioVerdict::Success { branch: "answered_despite_cancel" }),
        "got {verdict:?}"
    );
}

/// Fails unless `verdict` is the call ending on no accepted branch.
fn assert_no_accepted_ending(verdict: &ScenarioVerdict) {
    let ScenarioVerdict::Failure { cause: StepError::UnexpectedKind { detail, .. }, .. } = verdict
    else {
        panic!("a 2xx after the CANCEL was settled is no accepted ending, got {verdict:?}");
    };
    assert_eq!(detail, "the call ended on no accepted branch");
}

/// A 2xx held back past the CANCEL's 200 on a run that declares no loss
/// model: the judge stays strict, and the 2xx reads as arriving after the
/// CANCEL was settled. The fuzz lane's loss model never holds a 2xx to an
/// INVITE.
#[tokio::test(start_paused = true)]
async fn a_2xx_held_past_the_cancel_final_with_nothing_lost_is_no_accepted_ending() {
    let h = Harness::with_transit_delay_and_send_delay(
        "answer-held-past-the-cancel-final",
        SIMULATED_TRANSIT_DELAY_MS,
        hold_first_relayed_answer(),
    );
    assert_no_accepted_ending(&answer_crossing_the_callers_cancel(h).await);
}

/// A run that declares no loss model notes no loss, though its fabric loses
/// the 2xx's first copy: the 2xx reads as arriving after the CANCEL was
/// settled.
#[tokio::test(start_paused = true)]
async fn a_lost_2xx_on_a_run_declaring_no_loss_model_is_no_accepted_ending() {
    let net = SimulatedSignalingNetwork::new(SIMULATED_TRANSIT_DELAY_MS)
        .with_send_fate(lose_first_relayed_answer());
    let h = Harness::with_simulated_network("answer-lost-undeclared", net);
    assert_no_accepted_ending(&answer_crossing_the_callers_cancel(h).await);
}
