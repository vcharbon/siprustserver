//! **The caller's answer to a promoted offer, across a primary kill.** Under
//! the early-media promotion a caller whose INVITE made no offer is answered
//! with the callee's 183 promoted to a 200: that description is the callee's
//! offer, and her ACK carries the answer (RFC 3264 §4). The stack keeps it on
//! the call's replicated promotion state until the callee's 200, which
//! repeats the offer (RFC 3261 §13.2.1) and whose ACK carries it.
//!
//! ```text
//!   INVITE (no offer) → INVITE → 183 (offer, P-Early-Media)
//!          ← 200 (offer) ; ACK (answer)       ✗ primary killed ✗
//!                        → 200 (offer)  [bob's 2xx, at the survivor]
//!          → ACK (alice's answer) → BYE → 200
//! ```
//!
//! Incident: the primary killed, unannounced to the peers (no departure
//! sweep), after the turn that took alice's ACK replicated; bob's 200 takes
//! the call over on the survivor. The primary then reboots and reclaims.

use std::sync::Arc;
use std::time::Duration;

use crate::call_terminate_on_backup::reboot_and_reclaim;
use crate::departure_support::{spawn_reactive_proxy, spawn_workers_deciding};
use b2bua::decision::test_adapter::route_to_with_18x;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::store::envelope;
use call::features::RelayFirst18xStrategy;
use call::MsgpackCodec;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const EARLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

fn promoting() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to_with_18x(
                    "127.0.0.1",
                    5070,
                    RelayFirst18xStrategy::PromotePemTo200,
                ))
            })
            .build(),
    )
}

/// The caller's answer the backup's replica of `call_ref` keeps.
async fn replicated_answer(
    backup: &ReplicatedB2buaSut,
    primary: &str,
    call_ref: &str,
) -> Option<Vec<u8>> {
    let body = backup
        .get(PartitionRole::Backup, primary, call_ref)
        .await
        .expect("the backup holds a replica body for the call");
    let call = envelope::decode(&MsgpackCodec::new(), &body).expect("replica body decodes").call;
    call.promote_pem.and_then(|state| state.caller_answer)
}

#[tokio::test(start_paused = true)]
async fn the_survivor_acks_the_callees_200_with_the_callers_answer() {
    let mut fh = FailoverHarness::new("promoted-answer-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = spawn_reactive_proxy(&fh).await;
    let (mut w_b1, mut w_b2) = spawn_workers_deciding(&mut fh, promoting()).await;

    let mut call = alice.invite(&bob).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().sdp(), None, "a delayed offer");
    let (pri_ord, _) = worker_ordinals(uas.request());
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(EARLY.as_bytes()), "bob's offer, promoted");
    let mut dialog = call.ack_with(Some(OFFER)).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = {
        let survivor = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
        let call_ref =
            survivor.scan_one_backed_up(&pri_ord).await.expect("the call replicated to the backup");
        let kept = replicated_answer(survivor, &pri_ord, &call_ref).await;
        assert_eq!(kept.as_deref(), Some(OFFER.as_bytes()), "the backup keeps alice's answer");
        call_ref
    };

    fh.mark(&pri_ord, None, "crash", "primary down after taking alice's answer");
    {
        let primary = if pri_ord == "b1" { &mut w_b1 } else { &mut w_b2 };
        primary.crash();
    }
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    uas.respond(200, "OK").with_sdp(EARLY).await;
    let ack = bob.receive("ACK").await;
    let answer = ack.request().sdp().map(|sdp| String::from_utf8_lossy(sdp).into_owned());
    assert!(
        answer.as_deref().is_some_and(|sdp| sdp.contains("m=audio 10000")),
        "bob's ACK carries alice's answer: {answer:?}"
    );
    fh.advance(Duration::from_millis(300)).await;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;

    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "exactly one CDR for the failed-over call across the cluster",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
