//! **A swept call answered at its returned node inside the replication lag of
//! the backup copy's ring deadline.** The departed node returns and the callee
//! answers at it, but the node's writes reach the backup too late: the copy's
//! deadline fires first, so it CANCELs the callee and refuses the caller 480
//! after the 200 (the ADR-0047 D7 residual). The answered call stays up at the
//! returned node, the backup's replica heals to it, and the call survives the
//! node's next death on that replica (RFC 3261 §13.2.2.4: a non-2xx after the
//! 2xx ends no dialog).

use std::time::Duration;

use failover_harness::PartitionRole;
use scenario_harness::agent::Dialog;
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

use crate::departure_support::{kill_announced, B2};
use crate::inflight_resend::{reboot_and_reclaim, settle_to_one_cdr, REANSWER, REOFFER};
use crate::ringing_call_taken_over_on_departure::{depart, place, Callee, Placed};
use crate::swept_call_primary_returns::{answer, hang_up_and_settle, port, ready, reboot, sent};

/// The copy's wire deviations inside the lag, each the copy acting on a call
/// it never saw answered.
const LAG_DEVIATIONS: [(&str, &str); 2] = [
    ("no-cancel-after-final", "the copy CANCELs the callee after its answer"),
    ("unacked-invite-non-2xx-final", "the copy's 480 follows the caller's 200: no ACK takes it"),
];

/// Drain what the caller holds past its 200: the backup copy's 480, each copy
/// a repeat of one final its answered transaction no longer takes.
async fn drain_strays(p: &Placed) {
    while let Some(msg) = p.alice.take_queued().await {
        let stray = matches!(&msg, SipMessage::Response(r) if r.status() == 480);
        assert!(stray, "nothing but the copy's 480 reaches the caller: {msg:?}");
    }
}

/// The departed node returns 4 s before the copy's ring deadline, its writes
/// reaching the backup 2 s late, and the callee answers at it 1.5 s before the
/// deadline. Past the deadline, once the lag clears and the copy's strays end:
/// the copy CANCELled the callee and refused the caller, the returned node
/// keeps the answered call, and the RFC gate is whole again. Returns the
/// answered dialog.
async fn answered_inside_the_lag(p: &mut Placed) -> Dialog {
    let deadline = depart(p).await;
    let wait = deadline - p.fh.now_ms() - 4_000;
    p.fh.advance(Duration::from_millis(wait.max(0) as u64)).await;
    reboot(p).await;
    p.fh.delay("b1", "b2", 2_000);
    p.w_b2.simulate_peer_added("b1");
    ready(p).await;
    let answered = answer(p).await;
    assert!(p.fh.now_ms() < deadline, "the callee answers before the deadline");
    for (rule, why) in LAG_DEVIATIONS {
        p.fh.accept_rfc_deviations_from_now(rule, why);
    }

    let past = deadline - p.fh.now_ms() + 5_000;
    p.fh.advance(Duration::from_millis(past.max(0) as u64)).await;
    let b2 = port(B2);
    let cancels = sent(p, p.bob.addr(), b"CANCEL");
    assert!(!cancels.is_empty() && cancels.iter().all(|s| *s == b2), "the copy CANCELs the callee");
    let refusals = sent(p, p.alice.addr(), b"SIP/2.0 480");
    assert!(!refusals.is_empty() && refusals.iter().all(|s| *s == b2), "the copy refuses");
    // The callee's answered INVITE transaction is over: the CANCEL matches
    // nothing (RFC 3261 §9.2). Its retransmissions are one transaction.
    p.bob.receive("CANCEL").await.respond(481, "Call/Transaction Does Not Exist").await;
    drain_strays(p).await;

    p.fh.heal("b1", "b2");
    p.fh.advance(Duration::from_secs(40)).await;
    drain_strays(p).await;
    for (rule, _) in LAG_DEVIATIONS {
        p.fh.resume_rfc_gate(rule);
    }
    assert!(p.w_b1.serves(&p.ringing_ref), "the returned node keeps the answered call");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy is gone");
    assert!(sent(p, p.bob.addr(), b"BYE").is_empty(), "nobody ends the answered call");
    answered
}

/// The answered call stays up at the returned node: the copy's ending, which
/// reaches it once the lag clears, stands behind its answer and never ends
/// it; the call settles to one CDR when the caller hangs up.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_at_its_returned_node_inside_the_replication_lag_stays_up() {
    let mut p = place("swept-call-answered-inside-the-lag", Callee::Ringing).await;
    let answered = answered_inside_the_lag(&mut p).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// The backup's replica of the call, decoded: its state, whether its caller
/// was answered, and its `(p,b)`.
async fn replica(p: &Placed) -> (call::CallModelState, bool, (i64, i64)) {
    use call::CallBodyCodec;
    let body = p.w_b2.get(PartitionRole::Backup, "b1", &p.ringing_ref).await;
    let call: call::Call = call::MsgpackCodec::new()
        .decode(&body.expect("the backup holds a replica"))
        .expect("the replica decodes");
    let pb = call.topology.as_ref().map_or((0, 0), |t| (t.gen, t.bak_gen));
    (call.state, call::helpers::caller_answered(&call), pb)
}

/// After the lag, the backup's replica of the call heals to the returned
/// node's answered call; the caller re-INVITEs through the node, which then
/// dies for good. The caller's BYE reaches the callee through the backup,
/// which takes the answered call over from that replica, and the call settles
/// to one CDR once the node returns.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_inside_the_lag_survives_its_returned_nodes_death() {
    let mut p = place("swept-call-answered-inside-the-lag-node-dies", Callee::Ringing).await;
    let mut answered = answered_inside_the_lag(&mut p).await;
    let (state, caller_answered, _) = replica(&p).await;
    assert_eq!(state, call::CallModelState::Active, "the backup's replica heals to the live call");
    assert!(caller_answered, "and to its answer");

    let mut reinvite = answered.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut round = p.bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    round.respond(200, "OK").with_sdp(REANSWER).await;
    reinvite.expect_tolerating(200, &["OPTIONS"]).await;
    answered.ack(None).await;
    p.bob.receive("ACK").await;
    p.fh.advance(Duration::from_secs(1)).await;
    let (state, _, (gen, _)) = replica(&p).await;
    let live = p.w_b1.live_call(&p.ringing_ref).and_then(|c| c.topology).map(|t| t.gen);
    assert_eq!((state, Some(gen)), (call::CallModelState::Active, live), "it follows the round");

    kill_announced(&mut p.fh, &mut p.w_b1, &p.w_b2, "b1", &p.proxy).await;
    let mut bye = answered.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let mut quiet_bye = p.quiet.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    quiet_bye.expect_tolerating(200, &["OPTIONS"]).await;
    p.fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut p.fh, &mut p.w_b1, &p.w_b2, "b1", &p.proxy).await;
    for call_ref in [&p.ringing_ref, &p.quiet_ref] {
        settle_to_one_cdr(&p.fh, &[&p.alice, &p.bob], &[&p.w_b1, &p.w_b2], call_ref).await;
    }
}

/// The returned node's writes stop reaching the backup a second past the
/// deadline, for longer than a stream stays idle, and the caller re-INVITEs
/// through the node meanwhile. When the stream heals, the backup reads the
/// node's compacted log — one entry, the call at the node's newer `p` — over
/// the copy's unanswered ending: the answered call is not behind that ending,
/// so the backup's replica heals to it (ADR-0047 D7).
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_inside_the_lag_heals_its_replica_across_a_cut_stream() {
    let mut p = place("swept-call-answered-inside-the-lag-cut-stream", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    let wait = deadline - p.fh.now_ms() - 4_000;
    p.fh.advance(Duration::from_millis(wait.max(0) as u64)).await;
    reboot(&mut p).await;
    p.fh.delay("b1", "b2", 2_000);
    p.w_b2.simulate_peer_added("b1");
    ready(&mut p).await;
    let mut answered = answer(&mut p).await;
    for (rule, why) in LAG_DEVIATIONS {
        p.fh.accept_rfc_deviations_from_now(rule, why);
    }
    let at = deadline - p.fh.now_ms() + 1_000;
    p.fh.advance(Duration::from_millis(at.max(0) as u64)).await;
    p.fh.stall("b1", "b2");
    p.bob.receive("CANCEL").await.respond(481, "Call/Transaction Does Not Exist").await;
    p.fh.advance(Duration::from_secs(17)).await;
    drain_strays(&p).await;

    let mut reinvite = answered.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut round = p.bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    round.respond(200, "OK").with_sdp(REANSWER).await;
    reinvite.expect_tolerating(200, &["OPTIONS"]).await;
    answered.ack(None).await;
    p.bob.receive("ACK").await;
    p.fh.advance(Duration::from_secs(1)).await;
    let (state, _, _) = replica(&p).await;
    assert_ne!(state, call::CallModelState::Active, "the backup still holds the copy's ending");

    p.fh.resume("b1", "b2");
    p.fh.heal("b1", "b2");
    p.fh.advance(Duration::from_secs(5)).await;
    let (state, caller_answered, _) = replica(&p).await;
    assert_eq!(state, call::CallModelState::Active, "the backup's replica heals to the live call");
    assert!(caller_answered, "and to its answer");

    p.fh.advance(Duration::from_secs(40)).await;
    drain_strays(&p).await;
    for (rule, _) in LAG_DEVIATIONS {
        p.fh.resume_rfc_gate(rule);
    }
    hang_up_and_settle(p, answered, &[]).await;
}
