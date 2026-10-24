//! What the healed peers take once the cut lifts: the finals and CANCELs a
//! window carries, answered as the parties' own UAs would, and the wait for
//! the cluster's release of the call.

use std::time::Duration;

use failover_harness::{FailoverHarness, ReplicatedB2buaSut};
use scenario_harness::{Agent, Inbound};
use sip_message::SipMessage;

/// What crossing a window did on the wire: every INVITE final the caller took,
/// and the CANCELs the callee's answered INVITE drew.
pub(super) struct Crossing {
    pub(super) finals_to_alice: Vec<u16>,
    pub(super) cancels_to_bob: usize,
}

/// Pump `secs` seconds of virtual time, collecting what reaches the peers. The
/// callee's INVITE transaction took its 2xx, so a CANCEL reaching it draws a 481
/// (RFC 3261 §9.2); every non-2xx final the caller takes is hop-ACKed, as a real
/// UAC's transaction layer does (§17.1.1.3).
pub(super) async fn sweep(
    fh: &FailoverHarness,
    call: &scenario_harness::ClientInvite,
    alice: &Agent,
    bob: &Agent,
    secs: u64,
) -> Crossing {
    let mut out = Crossing { finals_to_alice: Vec::new(), cancels_to_bob: 0 };
    for _ in 0..secs {
        fh.advance(Duration::from_secs(1)).await;
        while let Some(mut cancel) = bob.try_receive_tolerating("CANCEL", &["OPTIONS", "ACK"]).await
        {
            out.cancels_to_bob += 1;
            cancel.respond(481, "Call/Transaction Does Not Exist").await;
        }
        while let Some(msg) = alice.take_queued().await {
            match msg {
                SipMessage::Response(r) if r.cseq().method().as_str() == "INVITE" => {
                    if r.status() >= 300 {
                        call.ack_non_2xx(&r).await.expect("the caller hop-ACKs the final");
                    }
                    out.finals_to_alice.push(r.status());
                }
                SipMessage::Response(_) => {}
                SipMessage::Request(r) => panic!("unexpected {} toward alice", r.method()),
            }
        }
    }
    out
}

/// Answer what the peers take once the SIP plane is healed: a keepalive OPTIONS
/// with 200, and a CANCEL with 481 — the callee's INVITE transaction took its
/// 2xx long ago (RFC 3261 §9.2). The CANCEL is the partitioned primary's: the
/// ring deadline's CANCEL client transaction outlives the copy it tore down and
/// keeps its Timer E ladder to Timer F (ADR-0034), so a retransmission crosses
/// as soon as the cut lifts. So does the primary's 480, which its INVITE server
/// transaction repeats on Timer G until the ACK or Timer H (§17.2.1): the
/// caller hop-ACKs it as every non-2xx final she takes (§17.1.1.3, as in
/// [`sweep`]). Returns how many CANCELs the callee answered.
pub(super) async fn answer_healed_peers(
    call: &scenario_harness::ClientInvite,
    alice: &Agent,
    bob: &Agent,
) -> usize {
    while let Some(inbound) = alice.take_queued_inbound().await {
        match inbound {
            Inbound::Request(mut t) if t.request().method() == "OPTIONS" => {
                t.respond(200, "OK").await;
            }
            Inbound::Response(r) if r.cseq().method() == "INVITE" && r.status() >= 300 => {
                call.ack_non_2xx(&r).await.expect("the caller hop-ACKs the final");
            }
            Inbound::Request(t) => panic!("unexpected {} toward alice", t.request().method()),
            Inbound::Response(r) => panic!("unexpected {} toward alice", r.status()),
        }
    }
    let mut cancels = 0;
    while let Some(mut cancel) = bob.try_receive_tolerating("CANCEL", &["OPTIONS"]).await {
        cancels += 1;
        cancel.respond(481, "Call/Transaction Does Not Exist").await;
    }
    cancels
}

/// Pump until no node holds a trace of `call_ref`, then assert the cluster-wide
/// release. Answers the healed peers along the way and returns the CANCELs the
/// callee took.
pub(super) async fn settle_released(
    fh: &FailoverHarness,
    call: &scenario_harness::ClientInvite,
    alice: &Agent,
    bob: &Agent,
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
) -> usize {
    let mut cancels = 0;
    let released = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(90), async || {
            cancels += answer_healed_peers(call, alice, bob).await;
            let mut clean = true;
            for n in nodes {
                if n.holds_any_trace(call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    assert!(released, "every node released {call_ref} within Timer H of the teardown");
    failover_harness::assert_call_fully_released(nodes, call_ref).await;
    // The answers sent on the last step cross before the wire is audited.
    fh.advance(Duration::from_millis(100)).await;
    cancels + answer_healed_peers(call, alice, bob).await
}
