//! The reliable-provisional give-up (RFC 3262 §3): a callee whose reliable
//! provisional no PRACK acknowledges within 64·T1 rejects the INVITE 5xx.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::actor::*;
use crate::agent::Inbound;
use crate::realcall::{CallCtx, CallScope};
use crate::{Harness, StepError, ANSWER_SDP, OFFER_SDP};

/// The caller's PRACK to bob's reliable 183 is lost on the wire, and every
/// later send of the caller with it until the INVITE's final arrives. Bob
/// rejects the INVITE 5xx 64·T1 after its 183, the caller ACKs the reject,
/// and bob's leg ends: no PRACK, no 200.
#[tokio::test(start_paused = true)]
async fn a_callee_rejects_5xx_when_its_reliable_183_is_never_pracked() {
    let alice_addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    let cut = Arc::new(AtomicBool::new(false));
    let fault: sip_net_testkit::SendFault = {
        let cut = cut.clone();
        Arc::new(move |src, _dst| {
            (cut.load(Ordering::SeqCst) && src == alice_addr).then(|| "PRACK lost".to_string())
        })
    };
    let h = Harness::with_transit_delay_and_send_fault("actor-reliable-give-up", 1, Some(fault))
        .describe(
            "RFC 3262 §3: alice's PRACK is lost; bob rejects the INVITE 5xx \
             64·T1 after its reliable 183 and alice ACKs the reject",
        );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let bob_actor = ActorState::from_spec(
        ActorSpec {
            role: "bob",
            agent: bob.clone(),
            disposition: Disposition::ReliableAnswer,
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![],
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),

            cseq: None,
            delayed: vec![],
            claim: None,
        },
        obs.clone(),
        Arc::new(CallScope::new()),
        &ctx,
        STEP_TIMEOUT,
        None,
        Automatics::default(),
        None,
        None,
    );

    let caller = Box::pin(async {
        let mut call =
            alice.invite(&bob).with_sdp(OFFER_SDP).with_header("Supported", "100rel").send().await;
        let p183 = call.try_expect(183).await?;
        let rang = Instant::now();
        cut.store(true, Ordering::SeqCst);
        let _ = call.try_prack(&p183).await;
        let deadline = rang + Duration::from_secs(40);
        loop {
            match alice.recv_any().await {
                Ok(Inbound::Response(r)) if r.status() >= 200 => {
                    cut.store(false, Ordering::SeqCst);
                    call.ack_non_2xx(&r).await?;
                    return Ok::<_, StepError>((r.status(), rang.elapsed()));
                }
                Ok(_) | Err(StepError::Timeout { .. }) => {}
                Err(e) => return Err(e),
            }
            assert!(Instant::now() < deadline, "bob never answered the INVITE a final");
        }
    });

    let (status, after) = tokio::select! {
        r = caller => r.expect("the caller's side of the call"),
        f = Box::pin(run_actor(bob_actor)) => panic!("bob's actor ended before any final: {f:?}"),
    };
    assert!((500..600).contains(&status), "the give-up final is a 5xx, got {status}");
    assert!(
        after >= 64 * T1 && after < 64 * T1 + Duration::from_secs(1),
        "the give-up comes 64·T1 after the reliable 183, it came after {after:?}"
    );
    h.advance(Duration::from_millis(10)).await;
    assert!(
        obs.with_snapshot(|s| s.leg("bob").phase() == LegPhase::Terminated),
        "bob's leg ends with its reject"
    );
    h.finish().await;
}
