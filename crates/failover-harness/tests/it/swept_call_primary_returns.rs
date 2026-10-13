//! **A ringing call taken over on its node's departure, whose node returns.**
//! The departed node comes back before the call's ring deadline and reclaims
//! the call from its own record while the backup's takeover copy is still live
//! (ADR-0047). One node ends or answers the call: the caller gets one final
//! response (RFC 3261 §13.2.2.4), the callee at most one CANCEL (§9.1), and the
//! call settles to one CDR — whether the callee stays silent, answers at the
//! returned node, answers at the backup while the node is down or not yet
//! routable, or the backup's copy wrote before the node came back.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{FailoverHarness, PartitionRole, WorkerHealth};
use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::Direction;
use scenario_harness::agent::Dialog;
use sip_message::SipMessage;

use crate::departure_support::{call_id_placed_on, B1, B2};
use crate::inflight_resend::settle_to_one_cdr;
use crate::ringing_call_taken_over_on_departure::{
    depart, final_at, place, place_ringing_for, ring_until, Callee, Placed, ANSWER, OFFER,
};

/// A ring deadline that outlives a changelog tombstone and the sweep that reaps
/// it, inside the call's setup timeout.
const LONG_RING: Duration = Duration::from_secs(140);
/// How long the replication fabric stays cut: past a tombstone's life (60 s in
/// the harness) and the next sweep that reaps it (every 30 s).
const PAST_TOMBSTONE: Duration = Duration::from_secs(100);

/// The SIP port of a worker, every incarnation alike.
fn port(worker: &str) -> u16 {
    worker.parse::<SocketAddr>().expect("a worker address").port()
}

/// Every message starting with `prefix` a worker sent toward `to`, by the
/// sender's port, in wire order.
fn sent(p: &Placed, to: SocketAddr, prefix: &[u8]) -> Vec<u16> {
    let proxy = p.proxy.addr();
    p.fh.sip_entries()
        .iter()
        .filter(|e| e.to == proxy && e.from != to && e.raw.starts_with(prefix))
        .map(|e| e.from.port())
        .collect()
}

/// The departed node reboots at a new address; the front proxy still reads it
/// dead and the backup has not seen it re-enter the membership.
async fn reboot(p: &mut Placed) {
    p.fh.mark("b1", None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = p.w_b1.reboot().await;
    p.proxy.set_address("b1", new_addr);
    p.fh.note_worker_rebound("b1", new_addr);
}

/// The departed node reboots at a new address and re-enters the membership;
/// the front proxy still reads it dead.
async fn rejoin(p: &mut Placed) {
    reboot(p).await;
    p.w_b2.simulate_peer_added("b1");
}

/// The rejoined node becomes ready and routable again.
async fn ready(p: &mut Placed) {
    for _ in 0..40 {
        p.fh.advance(Duration::from_millis(250)).await;
        if p.w_b1.is_ready() {
            break;
        }
    }
    assert!(p.w_b1.is_ready(), "the returning node became ready before the deadline");
    p.proxy.set_health("b1", WorkerHealth::Alive);
    assert!(p.w_b1.serves(&p.ringing_ref), "the returning node serves the call it reclaimed");
}

/// The departed node returns, reclaims the ringing call from its own record,
/// and becomes ready well before the call's deadline.
async fn return_before_deadline(p: &mut Placed) {
    rejoin(p).await;
    ready(p).await;
}

/// The callee's 200 relayed to the caller, and the caller's ACK to the callee.
async fn answer(p: &mut Placed) -> Dialog {
    p.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    p.ringing.expect(200).await;
    let answered = p.ringing.ack().await;
    p.bob.receive("ACK").await;
    answered
}

/// Past the ring deadline, the answered call holds its 200 alone: no copy
/// refuses the caller, no copy CANCELs the callee.
async fn answered_alone_past(p: &Placed, deadline: i64) {
    let past = deadline - p.fh.now_ms() + 5_000;
    p.fh.advance(Duration::from_millis(past.max(0) as u64)).await;
    assert!(
        sent(p, p.alice.addr(), b"SIP/2.0 480").is_empty(),
        "no copy refuses the answered call"
    );
    assert!(sent(p, p.bob.addr(), b"CANCEL").is_empty(), "no copy CANCELs the callee");
    while let Some(msg) = p.alice.take_queued().await {
        let final_ = matches!(&msg, SipMessage::Response(r) if r.status() >= 300);
        assert!(!final_, "the caller gets no final after its 200: {msg:?}");
    }
}

/// The established call ended by the caller; every call, `ended` included,
/// settles to one CDR. A keepalive OPTIONS the returned node sends on the
/// established call is answered 200 on the way (RFC 3261 §11.2).
async fn hang_up_quiet_and_settle(mut p: Placed, ended: &[String]) {
    let mut quiet_bye = p.quiet.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    quiet_bye.expect_tolerating(200, &["OPTIONS"]).await;
    p.fh.advance(Duration::from_secs(60)).await;
    for call_ref in [&p.ringing_ref, &p.quiet_ref].into_iter().chain(ended) {
        settle_to_one_cdr(&p.fh, &[&p.alice, &p.bob], &[&p.w_b1, &p.w_b2], call_ref).await;
    }
    drop(p.proxy);
}

/// Both calls ended by the caller; every call, `ended` included, settles to
/// one CDR.
async fn hang_up_and_settle(p: Placed, mut answered: Dialog, ended: &[String]) {
    let mut bye = answered.bye().await;
    p.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    hang_up_quiet_and_settle(p, ended).await;
}

/// The callee stays silent: at the deadline the returned primary alone ends
/// the call — one CANCEL to the callee, whose 487 it ACKs (§17.1.1.3), and one
/// 480 to the caller.
#[tokio::test(start_paused = true)]
async fn a_ringing_call_whose_node_returns_before_its_deadline_is_ended_by_its_primary_alone() {
    let mut p = place("ringing-call-node-returns", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    return_before_deadline(&mut p).await;

    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(&mut p, deadline).await;
    p.fh.advance(Duration::from_secs(40)).await;
    let b1 = port(B1);
    assert_eq!(sent(&p, p.bob.addr(), b"CANCEL"), [b1], "the primary alone CANCELs the callee");
    assert_eq!(
        sent(&p, p.alice.addr(), b"SIP/2.0 480"),
        [b1],
        "the primary alone refuses the caller"
    );
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The callee answers at the returned node, which relays its 200, and the
/// caller ACKs: the caller gets exactly one final, the 200, and the callee no
/// CANCEL.
#[tokio::test(start_paused = true)]
async fn a_ringing_call_whose_node_returns_and_is_then_answered_gets_no_final_but_its_200() {
    let mut p = place("ringing-call-node-returns-answered", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    return_before_deadline(&mut p).await;

    let answered = answer(&mut p).await;
    answered_alone_past(&p, deadline).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// The callee answers while the departed node is down: its 200 fails over to
/// the backup, whose takeover copy relays it and the caller's ACK. The node then
/// returns and reclaims the answered call: the caller gets the 200 alone.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_while_its_node_is_down_gets_no_final_but_its_200() {
    let mut p = place("swept-call-answered-node-down", Callee::Ringing).await;
    let deadline = depart(&mut p).await;

    let answered = answer(&mut p).await;
    assert_eq!(sent(&p, p.bob.addr(), b"ACK").last(), Some(&port(B2)), "the backup relays the ACK");
    return_before_deadline(&mut p).await;

    answered_alone_past(&p, deadline).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// The departed node has re-entered the membership but is not yet routable
/// when the callee answers: the front proxy still reads it dead, so the 200
/// fails over to the backup's takeover copy, which relays it and the ACK. The
/// node then becomes ready: the caller gets the 200 alone. On the paused
/// runtime the node reclaims the ringing call before the backup's answer
/// reaches it, and folds that answer as the backup's reverse flush; the answer
/// landing first is the node-down cell above.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_before_its_returning_node_is_routable_gets_no_final_but_its_200() {
    let mut p = place("swept-call-answered-node-rejoining", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    rejoin(&mut p).await;
    assert!(!p.w_b1.is_ready(), "the returning node is not ready yet");

    let answered = answer(&mut p).await;
    assert_eq!(sent(&p, p.bob.addr(), b"ACK").last(), Some(&port(B2)), "the backup relays the ACK");
    ready(&mut p).await;

    answered_alone_past(&p, deadline).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// The backup's takeover copy writes while the node is down: the callee's 183
/// with early media fails over to it, and it relays it.
async fn copy_writes_183(p: &mut Placed) {
    let authored = |p: &Placed| p.w_b2.call_bgen(PartitionRole::Backup, "b1", &p.ringing_ref);
    let before = authored(p);
    p.at_bob.respond(183, "Session Progress").with_sdp(ANSWER).await;
    p.ringing.expect(183).await;
    p.fh.advance(Duration::from_millis(500)).await;
    assert!(authored(p) > before, "the takeover copy wrote the callee's 183");
}

/// The node returns after its backup's copy wrote: its recovery pulls that
/// write, so its reclaim record holds everything the copy did, and the copy
/// stands down on it.
async fn return_over_the_copys_write(p: &mut Placed) {
    copy_writes_183(p).await;
    return_before_deadline(p).await;
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy stands down on the covering reclaim");
}

/// The backup's copy wrote before the node returned; the callee answers at
/// the returned node: the caller gets the 200 alone.
#[tokio::test(start_paused = true)]
async fn a_swept_call_whose_copy_wrote_and_whose_node_returns_then_answers_gets_no_final_but_its_200(
) {
    let mut p = place("swept-call-copy-wrote-node-returns", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    return_over_the_copys_write(&mut p).await;

    let primary_writes = |p: &Placed| p.w_b2.call_gen(PartitionRole::Backup, "b1", &p.ringing_ref);
    let ringing = primary_writes(&p);
    let answered = answer(&mut p).await;
    assert_eq!(
        sent(&p, p.bob.addr(), b"ACK").last(),
        Some(&port(B1)),
        "the primary relays the ACK"
    );
    p.fh.advance(Duration::from_millis(500)).await;
    assert!(primary_writes(&p) > ringing, "the backup holds the primary's answer");
    answered_alone_past(&p, deadline).await;
    hang_up_and_settle(p, answered, &[]).await;
}

/// The backup's copy wrote before the node returned; the callee stays
/// silent: at the deadline the returned primary alone CANCELs the callee,
/// ACKs its 487 and refuses the caller 480.
#[tokio::test(start_paused = true)]
async fn a_swept_call_whose_copy_wrote_and_whose_node_returns_is_ended_by_its_primary_alone() {
    let mut p = place("swept-call-copy-wrote-silent", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    return_over_the_copys_write(&mut p).await;

    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(&mut p, deadline).await;
    p.fh.advance(Duration::from_secs(40)).await;
    let b1 = port(B1);
    assert_eq!(sent(&p, p.bob.addr(), b"CANCEL"), [b1], "the primary alone CANCELs the callee");
    assert_eq!(
        sent(&p, p.alice.addr(), b"SIP/2.0 480"),
        [b1],
        "the primary alone refuses the caller"
    );
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The caller's final is the one it got, and nothing follows it to the past
/// of the ring deadline: no 480, and the callee's CANCELs, all the returned
/// node's, are `cancels`.
async fn one_final_past(p: &Placed, deadline: i64, cancels: &[u16]) {
    let past = deadline - p.fh.now_ms() + 5_000;
    p.fh.advance(Duration::from_millis(past.max(0) as u64)).await;
    assert!(sent(p, p.alice.addr(), b"SIP/2.0 480").is_empty(), "no 480 follows the final");
    let workers = [port(B1), port(B2)];
    let to_callee: Vec<u16> =
        sent(p, p.bob.addr(), b"CANCEL").into_iter().filter(|s| workers.contains(s)).collect();
    assert_eq!(to_callee, cancels, "the callee's CANCELs");
}

/// The backup's copy wrote before the node returned; the caller CANCELs at
/// the returned node, which CANCELs the callee and ACKs its 487: the caller's
/// 487 is its only final.
#[tokio::test(start_paused = true)]
async fn a_swept_call_whose_copy_wrote_and_whose_caller_cancels_at_its_returned_node_gets_one_final(
) {
    let mut p = place("swept-call-copy-wrote-caller-cancels", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    return_over_the_copys_write(&mut p).await;

    let mut cancel = p.ringing.cancel().await;
    cancel.expect(200).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    p.ringing.expect(487).await;
    one_final_past(&p, deadline, &[port(B1)]).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The backup's copy wrote before the node returned; the callee refuses at
/// the returned node, which relays its 486: the caller's 486 is its only
/// final and the callee gets no CANCEL.
#[tokio::test(start_paused = true)]
async fn a_swept_call_whose_copy_wrote_and_whose_callee_refuses_at_its_returned_node_gets_one_final(
) {
    let mut p = place("swept-call-copy-wrote-callee-refuses", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    return_over_the_copys_write(&mut p).await;

    p.at_bob.respond(486, "Busy Here").await;
    p.bob.receive("ACK").await;
    p.ringing.expect(486).await;
    one_final_past(&p, deadline, &[]).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The backup's stream from b1 falls off b1's compacted changelog: the
/// replication fabric is cut while a short call is placed and hung up on b1,
/// and stays cut past the life of that call's delete tombstone, so the healed
/// stream's resumed pull is answered with a reset to bootstrap. Returns the
/// short call's `call_ref` once the backup's stream from b1 holds a cold
/// watermark; the backup still holds its replica of the ringing call.
async fn fall_off_compacted_tail(p: &mut Placed) -> String {
    p.fh.partition("b1", "b2");
    let cid = call_id_placed_on("b1", "short");
    let mut call = p
        .alice
        .invite(&p.bob)
        .with_sdp(OFFER)
        .identity(cid, "short-tag")
        .through(p.proxy.addr())
        .send()
        .await;
    p.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut short = call.ack().await;
    p.bob.receive("ACK").await;
    let short_ref = p.w_b1.scan_primary("b1").into_iter().find(|r| r.contains("short"));
    let short_ref = short_ref.expect("b1 serves the short call");
    let mut bye = short.bye().await;
    p.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    p.fh.advance(PAST_TOMBSTONE).await;

    p.fh.heal("b1", "b2");
    let cold = Watermark::new(0, 0);
    for _ in 0..50 {
        p.fh.advance(Duration::from_millis(100)).await;
        if p.w_b2.backup_flow_watermark("b1") == cold {
            break;
        }
    }
    assert_eq!(p.w_b2.backup_flow_watermark("b1"), cold, "b1 reset the backup's stream");
    assert!(
        p.w_b2.scan_backed_up("b1").contains(&p.ringing_ref),
        "the backup still holds the ringing call's replica",
    );
    short_ref
}

/// Whether `caller` pulled `partition` from a cold watermark at or after
/// `from_ms` — a bootstrap scan, not a resumed tail.
fn pulled_cold(fh: &FailoverHarness, caller: &str, partition: Partition, from_ms: i64) -> bool {
    fh.repl_report().frames.iter().any(|f| {
        f.dir == Direction::Sent
            && f.at_ms >= from_ms
            && matches!(&f.frame, Frame::PullRequest { caller: c, partition: q, since, .. }
                if c == caller && *q == partition && *since == Watermark::new(0, 0))
    })
}

/// The backup's stream from the departed node restarts from a cold watermark,
/// so the returned node's reclaim reaches the backup inside the stream's
/// bootstrap scan rather than on its tail. The backup learns the node is back
/// only once the node is ready, so the scan carries the reclaimed call. The
/// backup's copy stands down on that scan, before the callee answers at the
/// returned node: the caller gets the 200 alone.
#[tokio::test(start_paused = true)]
async fn a_swept_call_whose_node_returns_into_a_cold_backup_stream_gets_no_final_but_its_200() {
    let mut p =
        place_ringing_for("swept-call-node-returns-cold-stream", Callee::Ringing, LONG_RING).await;
    let short_ref = fall_off_compacted_tail(&mut p).await;
    let deadline = depart(&mut p).await;
    reboot(&mut p).await;
    ready(&mut p).await;

    let rejoined_at = p.fh.now_ms();
    p.w_b2.simulate_peer_added("b1");
    p.fh.advance(Duration::from_millis(500)).await;
    assert!(
        pulled_cold(&p.fh, "b2", Partition::Bak, rejoined_at),
        "the backup bootstraps its stream from the returned node",
    );
    assert_ne!(p.w_b2.backup_flow_watermark("b1"), Watermark::new(0, 0), "and completes it");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the backup's copy stands down on the reclaim");

    let answered = answer(&mut p).await;
    answered_alone_past(&p, deadline).await;
    hang_up_and_settle(p, answered, &[short_ref]).await;
}

/// Drain what the caller holds past its 200: the backup copy's 480, each copy
/// a repeat of one final its answered transaction no longer takes.
async fn drain_strays(p: &Placed) {
    while let Some(msg) = p.alice.take_queued().await {
        let stray = matches!(&msg, SipMessage::Response(r) if r.status() == 480);
        assert!(stray, "nothing but the copy's 480 reaches the caller: {msg:?}");
    }
}

/// The returned node's writes reach the backup 2 s late, and the callee
/// answers at the returned node 1.5 s before the ring deadline: the backup's
/// copy has seen neither the reclaim nor the answer when its deadline fires,
/// so it CANCELs the callee and refuses the caller 480 after the 200 — the
/// residual of ADR-0047 D7. The call the caller and the callee hold stays up
/// at the returned node: the copy's ending, which reaches it once the lag
/// clears, stands behind its answer and never ends it; the call settles to
/// one CDR when the caller hangs up.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_at_its_returned_node_inside_the_replication_lag_stays_up() {
    let mut p = place("swept-call-answered-inside-the-lag", Callee::Ringing).await;
    let deadline = depart(&mut p).await;
    let wait = deadline - p.fh.now_ms() - 4_000;
    p.fh.advance(Duration::from_millis(wait.max(0) as u64)).await;
    reboot(&mut p).await;
    p.fh.delay("b1", "b2", 2_000);
    p.w_b2.simulate_peer_added("b1");
    ready(&mut p).await;
    let answered = answer(&mut p).await;
    assert!(p.fh.now_ms() < deadline, "the callee answers before the deadline");
    // What the copy puts on the wire inside the lag is what this cell measures.
    let lag = [
        ("no-cancel-after-final", "the copy CANCELs the callee after its answer"),
        (
            "unacked-invite-non-2xx-final",
            "the copy's 480 follows the caller's 200: no ACK takes it",
        ),
        ("no-bye-outside-or-early-dialog", "the copy's 480 reads as ending the answered dialog"),
    ];
    for (rule, why) in lag {
        p.fh.accept_rfc_deviations_from_now(rule, why);
    }

    let past = deadline - p.fh.now_ms() + 5_000;
    p.fh.advance(Duration::from_millis(past.max(0) as u64)).await;
    let b2 = port(B2);
    let cancels = sent(&p, p.bob.addr(), b"CANCEL");
    assert!(!cancels.is_empty() && cancels.iter().all(|s| *s == b2), "the copy CANCELs the callee");
    let refusals = sent(&p, p.alice.addr(), b"SIP/2.0 480");
    assert!(!refusals.is_empty() && refusals.iter().all(|s| *s == b2), "the copy refuses");
    // The callee's answered INVITE transaction is over: the CANCEL matches
    // nothing (RFC 3261 §9.2). Its retransmissions are one transaction.
    p.bob.receive("CANCEL").await.respond(481, "Call/Transaction Does Not Exist").await;
    drain_strays(&p).await;

    p.fh.heal("b1", "b2");
    p.fh.advance(Duration::from_secs(40)).await;
    drain_strays(&p).await;
    assert!(p.w_b1.serves(&p.ringing_ref), "the returned node keeps the answered call");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the copy is gone");
    assert!(sent(&p, p.bob.addr(), b"BYE").is_empty(), "nobody ends the answered call");
    hang_up_and_settle(p, answered, &[]).await;
}
