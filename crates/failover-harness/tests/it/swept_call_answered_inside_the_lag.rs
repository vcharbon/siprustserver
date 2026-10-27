//! **A swept call answered at its returned node inside the replication lag of
//! the backup copy's ring deadline.** The departed node returns and the callee
//! answers at it, but the node's writes reach the backup too late: the copy's
//! deadline fires first, so it CANCELs the callee and refuses the caller 480
//! after the 200 (the ADR-0047 D7 residual). The returned node's answer and the
//! copy's ending are each a write the other never read: the two copies
//! diverged (ADR-0055 §7), and once the lag clears the returned node meets the
//! copy's ending and ends the answered call cleanly from its own copy, ranked
//! higher on the answer — a BYE to each party (RFC 3261 §13.2.2.4: a non-2xx
//! after the 2xx ends no dialog, so the dialog the 200 made is BYEd), one CDR.

use std::time::Duration;

use failover_harness::Transitional;
use scenario_harness::{Agent, Inbound};
use sip_message::SipMessage;

use crate::departure_support::B2;
use crate::ringing_call_taken_over_on_departure::{depart, place, Callee, Placed};
use crate::swept_call_primary_returns::{
    answer, hang_up_quiet_and_settle, port, ready, reboot, sent,
};

/// Drain what the caller holds past its 200: the backup copy's 480, each copy
/// a repeat of one final its answered transaction no longer takes.
async fn drain_strays(p: &Placed) {
    while let Some(msg) = p.alice.take_queued().await {
        let stray = matches!(&msg, SipMessage::Response(r) if r.status() == 480);
        assert!(stray, "nothing but the copy's 480 reaches the caller: {msg:?}");
    }
}

/// Answer what reaches `party` while the call ends: a BYE `200`, a stale
/// CANCEL `481`, the copy's 480 left alone. Returns the BYEs it took.
async fn answer_the_end(party: &Agent) -> usize {
    let mut byes = 0;
    while let Some(inbound) = party.take_queued_inbound().await {
        match inbound {
            Inbound::Request(mut txn) => match txn.request().method().as_str() {
                "BYE" => {
                    byes += 1;
                    txn.respond(200, "OK").await;
                }
                "CANCEL" => txn.respond(481, "Call/Transaction Does Not Exist").await,
                other => panic!("unexpected {other} while the call ends"),
            },
            Inbound::Response(r) => assert_eq!(r.status(), 480, "only the copy's 480 strays"),
        }
    }
    byes
}

/// The departed node returns 4 s before the copy's ring deadline, its writes
/// reaching the backup 2 s late, and the callee answers at it 1.5 s before the
/// deadline. Past the deadline: the copy CANCELled the callee and refused the
/// caller. Then the returned node ends the answered call, each party taking a
/// BYE, and the call settles to one CDR.
async fn answered_inside_the_lag_then_ended(mut p: Placed, stall_after_deadline: bool) {
    let deadline = depart(&mut p).await;
    let wait = deadline - p.fh.now_ms() - 4_000;
    p.fh.advance(Duration::from_millis(wait.max(0) as u64)).await;
    reboot(&mut p).await;
    p.fh.delay("b1", "b2", 2_000);
    p.w_b2.simulate_peer_added("b1");
    ready(&mut p).await;
    let _answered = answer(&mut p).await;
    assert!(p.fh.now_ms() < deadline, "the callee answers before the deadline");
    // The copy acts on a call it never saw answered: it CANCELs the callee
    // after its answer and refuses the caller after her 200.
    p.fh.strike(Transitional::AnsweredInsideTheLag);
    if stall_after_deadline {
        let at = deadline - p.fh.now_ms() + 1_000;
        p.fh.advance(Duration::from_millis(at.max(0) as u64)).await;
        p.fh.stall("b1", "b2");
        p.fh.advance(Duration::from_secs(17)).await;
        p.fh.resume("b1", "b2");
    }

    let mut byes = (0, 0);
    let mut waited = Duration::ZERO;
    while (byes.0 == 0 || byes.1 == 0) && waited < Duration::from_secs(60) {
        p.fh.advance(Duration::from_millis(200)).await;
        waited += Duration::from_millis(200);
        byes.0 += answer_the_end(&p.alice).await;
        byes.1 += answer_the_end(&p.bob).await;
    }
    assert!(byes.0 > 0 && byes.1 > 0, "the returned node BYEs each party");
    let b2 = port(B2);
    let cancels = sent(&p, p.bob.addr(), b"CANCEL");
    assert!(!cancels.is_empty() && cancels.iter().all(|s| *s == b2), "the copy CANCELs the callee");
    let refusals = sent(&p, p.alice.addr(), b"SIP/2.0 480");
    assert!(!refusals.is_empty() && refusals.iter().all(|s| *s == b2), "the copy refuses");

    p.fh.heal("b1", "b2");
    p.fh.advance(Duration::from_secs(40)).await;
    answer_the_end(&p.alice).await;
    answer_the_end(&p.bob).await;
    drain_strays(&p).await;
    p.fh.incident_over();
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The copy's ending reaches the returned node once the lag clears.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_at_its_returned_node_inside_the_lag_ends_cleanly() {
    let p = place("swept-call-answered-inside-the-lag", Callee::Ringing).await;
    answered_inside_the_lag_then_ended(p, false).await;
}

/// The returned node's writes stop reaching the backup a second past the
/// deadline, for longer than a stream stays idle; when the stream heals, the
/// two copies meet the same way.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_inside_the_lag_ends_cleanly_across_a_cut_stream() {
    let p = place("swept-call-answered-inside-the-lag-cut-stream", Callee::Ringing).await;
    answered_inside_the_lag_then_ended(p, true).await;
}
