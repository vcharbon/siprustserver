//! The `long_call` hold follows its one OPTIONS ping: the caller's BYE leaves
//! `long_hold` after the ping's final, so a ping the network lost and the
//! caller's Timer E recovers (RFC 3261 §17.1.2.2) is answered before the BYE
//! ends the dialog, and the callee takes the dialog's requests in CSeq order
//! (§12.2.1.1).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{Arrival, PreIngressAction};

use crate::actor::scenarios::LongCall;
use crate::actor::*;
use crate::realcall::{CallCtx, CallEnv};
use crate::Harness;

/// The `sent_ms` and start line of the first recorded message `pick` selects.
fn first_sent(
    entries: &[sip_net_testkit::RecordedSipEntry],
    pick: impl Fn(&SipMessage) -> bool,
) -> Option<(u64, String)> {
    entries.iter().find_map(|e| {
        let msg = CustomParser::new().parse(&e.raw).ok()?;
        let line = String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string();
        pick(&msg).then_some((e.sent_ms, line))
    })
}

#[tokio::test(start_paused = true)]
async fn the_bye_waits_for_the_answer_to_a_lost_ping() {
    let h = Harness::new("actor-long-call-lost-ping").describe(
        "long_call with a 100 ms hold: the first copy of alice's OPTIONS ping is \
         lost before bob, Timer E resends it at T1, bob answers it, and alice's \
         BYE leaves 100 ms after that 200",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let copies = Arc::new(AtomicUsize::new(0));
    let counter = copies.clone();
    let bob = h
        .agent_with_pre_ingress(
            "bob",
            "127.0.0.1:5070",
            Arc::new(move |arrival: &mut Arrival, _src, _depth| {
                if arrival.raw().starts_with(b"OPTIONS ")
                    && counter.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;
    let mut env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Corr", "t");
    env.long_hold = Duration::from_millis(100);

    let verdict = run_actor_scenario(&LongCall, &env, &CallCtx::new()).await;
    assert_eq!(verdict.branch(), Some("answered"), "{verdict:?}");
    assert_eq!(copies.load(Ordering::SeqCst), 2, "the lost ping went again once");

    let entries = h.wire_entries();
    let (answered, final_line) = first_sent(&entries, |m| {
        matches!(m, SipMessage::Response(r) if r.status() >= 200 && r.cseq().method() == "OPTIONS")
    })
    .expect("bob answered the ping");
    assert_eq!(final_line, "SIP/2.0 200 OK", "bob answered the ping on a live dialog");
    let (bye, _) =
        first_sent(&entries, |m| matches!(m, SipMessage::Request(r) if r.method() == "BYE"))
            .expect("alice sent her BYE");
    assert!(
        bye >= answered + 100,
        "the BYE ({bye} ms) left before the hold that follows the ping's 200 ({answered} ms)"
    );
    h.finish().await;
}
