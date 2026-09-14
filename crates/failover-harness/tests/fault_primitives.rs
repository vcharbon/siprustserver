//! **Every fault primitive bites on the fabric.**
//!
//! A fault primitive is a harness verb that changes what the simulated fabric
//! delivers. A primitive that does not bite passes every scenario written on
//! it, so each one carries a self-test whose assertion is on the fabric's
//! delivery — the captured `Sent`/`Received` pairs of the replication
//! recording, the `delivered` flag of the SIP recording — never only on the
//! SUT's eventual outcome:
//!
//! - a partition delivers **nothing** across the cut until the heal, then
//!   everything it held lands in order;
//! - a delay lands a frame **no earlier than** `sent + ms`, on the streams
//!   that exist when it is applied and on any stream opened afterwards;
//! - a signalling cut delivers **no datagram** to or from the cut address;
//! - a crash stops every frame and every datagram from the node;
//! - a partition holds a **replacement incarnation** of a node as it holds
//!   the original.
//!
//! Clock: everything runs on the paused clock, both fabrics carry a 1 ms
//! transit, and the harness pumps time in 100 ms chunks. A "no earlier than"
//! verdict reads the recording's `at_ms` of the two captures of one frame,
//! never a sleep. A verdict on "everything landed, in order" reads the FIFO
//! position of a frame at the sender and at the receiver.
//!
//! Every test drives one call to a proper end — BYE answered, one CDR across
//! the cluster, no trace and clean per-call memory on every node — under the
//! harness's RFC gate.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Duration;

use call::{CallBodyCodec, LegState, MsgpackCodec};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use ha_harness::{frame_summary, ReplReport};
use repl_net::frame::{Frame, Partition};
use repl_net::transport::{CapturedFrame, Direction};
use scenario_harness::Agent;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const BAK: PartitionRole = PartitionRole::Backup;
const PRI: PartitionRole = PartitionRole::Primary;

/// The delay every delay cell applies, in ms — well above the 100 ms pump
/// chunk, so "landed early" and "landed on time" are chunks apart.
const DELAY_MS: u64 = 3_000;

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

/// Two replicating workers behind the proxy, both ready, both sending their
/// b-leg back through the proxy.
async fn spawn_cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");
    Cluster { fh, alice, bob, proxy, w_b1, w_b2 }
}

/// The node that serves the call and the node that backs it up, the serving
/// one mutable.
fn split_mut<'a>(
    pri_ord: &str,
    w_b1: &'a mut ReplicatedB2buaSut,
    w_b2: &'a mut ReplicatedB2buaSut,
) -> (&'a mut ReplicatedB2buaSut, &'a ReplicatedB2buaSut) {
    if pri_ord == "b1" {
        (w_b1, &*w_b2)
    } else {
        (w_b2, &*w_b1)
    }
}

/// The backup mutable, the primary not — for the cells that kill the backup.
fn split_backup_mut<'a>(
    pri_ord: &str,
    w_b1: &'a mut ReplicatedB2buaSut,
    w_b2: &'a mut ReplicatedB2buaSut,
) -> (&'a ReplicatedB2buaSut, &'a mut ReplicatedB2buaSut) {
    if pri_ord == "b1" {
        (&*w_b1, w_b2)
    } else {
        (&*w_b2, w_b1)
    }
}

/// The first replicated ref in `bak:{primary}` on the backup.
async fn find_backed_up_ref(backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary).await {
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the backup");
}

/// The replica body the backup holds for `call_ref`, decoded.
async fn replica(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) -> call::Call {
    let body = backup.get(BAK, primary, call_ref).await.expect("replica body present");
    MsgpackCodec::new().decode(&body).expect("replica body decodes")
}

/// The recording-order seq of the last marker the harness appended — the
/// boundary a "since the fault" filter reads. Frames and SIP entries draw
/// from the same sequencer.
fn last_marker_seq(fh: &FailoverHarness) -> u64 {
    fh.repl_report().markers.last().expect("a marker was appended").seq
}

/// The node every address on the replication fabric belongs to: a lane (a
/// listen address) is its own node, a puller's ephemeral local is the node
/// the `caller` of the `PullRequest` it opened with names.
fn owners(report: &ReplReport) -> BTreeMap<SocketAddr, String> {
    let mut map = report.lanes.clone();
    for f in &report.frames {
        if let Frame::PullRequest { caller, .. } = &f.frame {
            if f.dir == Direction::Sent {
                map.entry(f.from).or_insert_with(|| caller.clone());
            }
        }
    }
    map
}

/// The listen address of `ordinal`'s current incarnation as the lane map
/// names it — the highest port when a replacement listens beside the original.
fn listen_of(report: &ReplReport, ordinal: &str) -> SocketAddr {
    report
        .lanes
        .iter()
        .filter(|(_, ord)| ord.as_str() == ordinal)
        .map(|(addr, _)| *addr)
        .max_by_key(|a| a.port())
        .unwrap_or_else(|| panic!("{ordinal} has a replication lane"))
}

/// Every address `ordinal` owns on the fabric: its listen lane(s) and the
/// ephemeral locals its pullers opened streams from.
fn owned_by(report: &ReplReport, ordinal: &str) -> BTreeSet<SocketAddr> {
    owners(report).into_iter().filter(|(_, ord)| ord == ordinal).map(|(addr, _)| addr).collect()
}

/// Every frame **received** on a connection between node `a` and node `b`
/// captured after `since` — what the fabric delivered across the pair.
fn received_between(report: &ReplReport, a: &str, b: &str, since: u64) -> Vec<CapturedFrame> {
    let owners = owners(report);
    report
        .frames
        .iter()
        .filter(|f| f.seq > since && f.dir == Direction::Received)
        .filter(|f| {
            let (fo, to) = (owners.get(&f.from), owners.get(&f.to));
            matches!((fo, to), (Some(x), Some(y)) if (x == a && y == b) || (x == b && y == a))
        })
        .cloned()
        .collect()
}

/// The client addresses `listener` serves a stream to — every puller that
/// opened one, present or past.
fn clients_of(report: &ReplReport, listener: SocketAddr) -> BTreeSet<SocketAddr> {
    report
        .frames
        .iter()
        .filter(|f| f.from == listener && f.dir == Direction::Sent)
        .map(|f| f.to)
        .collect()
}

/// One frame's two captures on the wire `src → dst`: what `src` handed to
/// `send`, and — if it landed — what `dst` took out of `recv`.
struct Transit {
    sent: CapturedFrame,
    received: Option<CapturedFrame>,
}

/// The transits of the wire `src → dst`, paired by FIFO position: the fabric
/// is ordered, so the receiver's sequence is a prefix of the sender's. A
/// receive that does not match the send at its position is a reordering, and
/// panics here.
fn transits(report: &ReplReport, src: SocketAddr, dst: SocketAddr) -> Vec<Transit> {
    let sent: Vec<&CapturedFrame> = report
        .frames
        .iter()
        .filter(|f| f.from == src && f.to == dst && f.dir == Direction::Sent)
        .collect();
    let received: Vec<&CapturedFrame> = report
        .frames
        .iter()
        .filter(|f| f.from == dst && f.to == src && f.dir == Direction::Received)
        .collect();
    assert!(
        received.len() <= sent.len(),
        "{src} → {dst}: {} frames received, only {} sent",
        received.len(),
        sent.len()
    );
    sent.iter()
        .enumerate()
        .map(|(i, s)| {
            let r = received.get(i).copied();
            if let Some(r) = r {
                assert_eq!(
                    r.frame,
                    s.frame,
                    "{src} → {dst}: frame #{i} was received out of order (sent {}, received {})",
                    frame_summary(&s.frame),
                    frame_summary(&r.frame)
                );
            }
            Transit { sent: (*s).clone(), received: r.cloned() }
        })
        .collect()
}

/// Whether a frame is a `Data` frame (a flush), as opposed to the keepalive
/// and handshake traffic.
fn is_data(frame: &Frame) -> bool {
    matches!(frame, Frame::Data { .. })
}

/// Pump until no node holds a trace of `call_ref`, then assert the
/// cluster-wide release with exactly one CDR. Answers keepalive OPTIONS along
/// the way.
async fn settle_to_one_cdr(
    fh: &FailoverHarness,
    peers: &[&Agent],
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
) {
    let drained = fh
        .settle_terminal(async || {
            for p in peers {
                if let Some(mut t) = p.try_receive_tolerating("OPTIONS", &[]).await {
                    t.respond(200, "OK").await;
                }
            }
            let mut clean = true;
            for n in nodes {
                if !n.memory_clean() || n.holds_any_trace(call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    assert!(drained, "the cluster releases {call_ref} within the settle budget");
    fh.linger_peers(peers, Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(nodes, call_ref), 1, "exactly one CDR across the cluster");
    assert_call_fully_released(nodes, call_ref).await;
}

/// Reboot a crashed node empty at a higher gen and a new SIP address, drive
/// it ready and let the proxy read it alive again.
async fn reboot_and_ready(
    fh: &mut FailoverHarness,
    node: &mut ReplicatedB2buaSut,
    peer: &ReplicatedB2buaSut,
    ordinal: &str,
    proxy: &ProxySut,
) {
    fh.mark(ordinal, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = node.reboot().await;
    proxy.set_address(ordinal, new_addr);
    fh.note_worker_rebound(ordinal, new_addr);
    peer.simulate_peer_added(ordinal);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if node.is_ready() {
            break;
        }
    }
    assert!(node.is_ready(), "rebooted {ordinal} became ready");
    proxy.set_health(ordinal, WorkerHealth::Alive);
}

/// The caller's in-dialog re-INVITE, answered end to end.
async fn reinvite(fh: &FailoverHarness, dialog: &mut scenario_harness::Dialog, bob: &Agent) {
    let mut round =
        dialog.request(sip_message::generators::InDialogMethod::Invite, Some(REOFFER)).await;
    let mut peer = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    peer.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect_tolerating(200, &["OPTIONS"]).await;
    dialog.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
}

/// **A partition delivers nothing across the cut.** The callee answers while
/// the primary is partitioned from its backup: the primary flushes the
/// answer, the fabric holds it — zero frames received on any connection
/// between the two nodes, the backup replica still the ringing body. The heal
/// releases everything the cut held, in order, and the replica converges.
#[tokio::test(start_paused = true)]
async fn partition_delivers_nothing_across_the_cut() {
    let name = "fault-partition-delivers-nothing";
    let Cluster { mut fh, alice, bob, proxy, w_b1, w_b2 } = spawn_cluster(name).await;

    // ── ring: the ringing version reaches the backup ─────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(backup, &pri_ord).await;
    let ringing = replica(backup, &pri_ord, &call_ref).await;
    assert_ne!(ringing.a_leg.state, LegState::Confirmed, "the backup holds the ringing a-leg");
    let ringing_p = backup.call_gen(BAK, &pri_ord, &call_ref).expect("the replica has a (p,b)");
    let pri_listen = listen_of(&fh.repl_report(), &pri_ord);

    // ── the cut, then the primary mutates: the callee answers ────────────────
    fh.partition(&pri_ord, &bak_ord);
    let cut_seq = last_marker_seq(&fh);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_secs(2)).await;
    let live = primary.live_call(&call_ref).expect("the primary serves the call");
    assert_eq!(live.a_leg.state, LegState::Confirmed, "the primary answered the caller");

    // ── the fabric: the answer was flushed, nothing crossed ──────────────────
    let report = fh.repl_report();
    let staged: Vec<&CapturedFrame> = report
        .frames
        .iter()
        .filter(|f| f.seq > cut_seq && f.dir == Direction::Sent && f.from == pri_listen)
        .filter(|f| is_data(&f.frame))
        .collect();
    assert!(!staged.is_empty(), "the primary flushed the answer version behind the cut");
    let crossed = received_between(&report, &pri_ord, &bak_ord, cut_seq);
    assert!(
        crossed.is_empty(),
        "the partition delivered {} frame(s) between {pri_ord} and {bak_ord}; the first: {:?}",
        crossed.len(),
        crossed.first().map(|f| format!("{} → {}: {}", f.from, f.to, frame_summary(&f.frame))),
    );
    let held = replica(backup, &pri_ord, &call_ref).await;
    assert_eq!(
        (held.state, held.a_leg.state),
        (ringing.state, ringing.a_leg.state),
        "the backup replica is still the ringing body while the cut lasts",
    );
    assert_eq!(
        backup.call_gen(BAK, &pri_ord, &call_ref),
        Some(ringing_p),
        "the replica's version did not move across the cut",
    );

    // ── the heal: everything the cut held lands, in order ────────────────────
    fh.heal(&pri_ord, &bak_ord);
    fh.advance(Duration::from_secs(2)).await;
    let report = fh.repl_report();
    let mut landed = 0;
    for client in clients_of(&report, pri_listen) {
        for t in transits(&report, pri_listen, client).iter().filter(|t| t.sent.seq > cut_seq) {
            assert!(
                t.received.is_some(),
                "a frame staged behind the cut never landed after the heal: {}",
                frame_summary(&t.sent.frame)
            );
            landed += 1;
        }
    }
    assert!(landed >= staged.len(), "every frame the cut held landed ({landed} frames)");
    let converged = replica(backup, &pri_ord, &call_ref).await;
    assert_eq!(converged.a_leg.state, LegState::Confirmed, "the replica converged on the heal");
    assert!(
        backup.call_gen(BAK, &pri_ord, &call_ref).is_some_and(|p| p > ringing_p),
        "the replica carries the answered version",
    );

    // ── the call ends end-to-end and the cluster releases it ─────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// **A delay lands a frame no earlier than the delay.** With the primary's
/// streams toward the backup delayed by 3 s, the answer the primary flushes
/// is received by the backup at `sent + 3000` or later — the captured
/// timestamps of the two ends say so — and at every pump chunk before that
/// instant the replica is still the ringing body.
#[tokio::test(start_paused = true)]
async fn delay_lands_a_frame_no_earlier_than_the_delay() {
    let name = "fault-delay-no-earlier";
    let Cluster { mut fh, alice, bob, proxy, w_b1, w_b2 } = spawn_cluster(name).await;

    // ── ring: the ringing version reaches the backup ─────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(backup, &pri_ord).await;
    let ringing = replica(backup, &pri_ord, &call_ref).await;
    assert_ne!(ringing.a_leg.state, LegState::Confirmed, "the backup holds the ringing a-leg");
    let pri_listen = listen_of(&fh.repl_report(), &pri_ord);

    // ── the delay, then the primary flushes: the callee answers ──────────────
    fh.delay_streams_from(&pri_ord, &bak_ord, DELAY_MS);
    let delay_seq = last_marker_seq(&fh);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(100)).await;

    // The first flush the primary wrote after the marker, and when it left.
    let sent_at = fh
        .repl_report()
        .frames
        .iter()
        .filter(|f| f.seq > delay_seq && f.dir == Direction::Sent && f.from == pri_listen)
        .filter(|f| is_data(&f.frame))
        .map(|f| f.at_ms)
        .min()
        .expect("the primary flushed the answer version after the delay was applied");
    let lands_at = sent_at + DELAY_MS as i64;

    // ── before the delay elapses, chunk by chunk: nothing lands, the old body
    while fh.now_ms() + 200 < lands_at {
        fh.advance(Duration::from_millis(100)).await;
        assert!(fh.now_ms() < lands_at, "the probe stays before the delivery instant");
        let early: Vec<CapturedFrame> = fh
            .repl_report()
            .frames
            .into_iter()
            .filter(|f| f.seq > delay_seq && f.dir == Direction::Received && f.to == pri_listen)
            .collect();
        assert!(
            early.is_empty(),
            "at {} ms (flush sent at {sent_at} ms, delay {DELAY_MS} ms) {} frame(s) from \
             {pri_listen} already landed; the first: {}",
            fh.now_ms(),
            early.len(),
            frame_summary(&early[0].frame),
        );
        let held = replica(backup, &pri_ord, &call_ref).await;
        assert_eq!(
            (held.state, held.a_leg.state),
            (ringing.state, ringing.a_leg.state),
            "at {} ms (flush sent at {sent_at} ms, delay {DELAY_MS} ms) the replica is still \
             the ringing body",
            fh.now_ms(),
        );
    }

    // ── then it lands, and no frame of the delayed wire landed early ─────────
    fh.advance(Duration::from_secs(1)).await;
    let report = fh.repl_report();
    let mut checked = 0;
    for client in clients_of(&report, pri_listen) {
        for t in transits(&report, pri_listen, client).iter().filter(|t| t.sent.seq > delay_seq) {
            let Some(r) = &t.received else { continue };
            assert!(
                r.at_ms >= t.sent.at_ms + DELAY_MS as i64,
                "a frame on {pri_listen} → {client} landed {} ms after it was sent; the delay is \
                 {DELAY_MS} ms: {}",
                r.at_ms - t.sent.at_ms,
                frame_summary(&t.sent.frame),
            );
            checked += 1;
        }
    }
    assert!(checked >= 1, "at least one delayed frame landed ({checked} checked)");
    let converged = replica(backup, &pri_ord, &call_ref).await;
    assert_eq!(converged.a_leg.state, LegState::Confirmed, "the delayed answer version landed");

    // ── the call ends end-to-end and the cluster releases it ─────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// **A delay reaches a stream opened after the fault.** The delay is applied,
/// then the receiving node crashes and reboots: its pullers reconnect to the
/// primary from fresh ephemeral locals, and the bootstrap frames the primary
/// serves on those new streams land no earlier than `sent + 3000`.
#[tokio::test(start_paused = true)]
async fn delay_reaches_a_stream_opened_after_the_fault() {
    let name = "fault-delay-reaches-new-stream";
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = spawn_cluster(name).await;

    // ── ring: the ringing version reaches the backup ─────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    let (primary, backup) = split_backup_mut(&pri_ord, &mut w_b1, &mut w_b2);
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(backup, &pri_ord).await;
    let pri_listen = listen_of(&fh.repl_report(), &pri_ord);

    // ── the delay, then the receiving node dies and comes back ───────────────
    fh.delay_streams_from(&pri_ord, &bak_ord, DELAY_MS);
    fh.mark(&bak_ord, None, "crash", "the backup dies under the delay");
    backup.crash();
    proxy.set_health(&bak_ord, WorkerHealth::Dead);
    primary.simulate_peer_removed(&bak_ord);
    fh.advance(Duration::from_millis(300)).await;
    reboot_and_ready(&mut fh, backup, primary, &bak_ord, &proxy).await;
    let reboot_seq = fh
        .repl_report()
        .markers
        .iter()
        .find(|m| m.kind == "reboot")
        .expect("the reboot marker")
        .seq;

    // ── the streams the rebooted node opened: every bootstrap frame is late ──
    let report = fh.repl_report();
    let fresh: Vec<SocketAddr> = clients_of(&report, pri_listen)
        .into_iter()
        .filter(|client| {
            report.frames.iter().any(|f| {
                f.seq > reboot_seq
                    && f.from == *client
                    && f.to == pri_listen
                    && f.dir == Direction::Sent
                    && matches!(f.frame, Frame::PullRequest { .. })
            })
        })
        .collect();
    assert!(!fresh.is_empty(), "the rebooted {bak_ord} reopened its streams to {pri_ord}");
    let mut data_checked = 0;
    for client in &fresh {
        for t in transits(&report, pri_listen, *client) {
            let Some(r) = &t.received else { continue };
            assert!(
                r.at_ms >= t.sent.at_ms + DELAY_MS as i64,
                "a frame on the stream {pri_listen} → {client} opened after the fault landed {} \
                 ms after it was sent; the delay is {DELAY_MS} ms: {}",
                r.at_ms - t.sent.at_ms,
                frame_summary(&t.sent.frame),
            );
            if is_data(&t.sent.frame) {
                data_checked += 1;
            }
        }
    }
    assert!(data_checked >= 1, "the bootstrap carried the ringing call's body on a fresh stream");
    assert!(
        backup.get(BAK, &pri_ord, &call_ref).await.is_some(),
        "the rebooted backup re-hydrated the ringing call",
    );

    // ── the call ends end-to-end and the cluster releases it ─────────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// **A signalling cut delivers no datagram to or from the node.** The cut
/// lands while the worker's b-leg INVITE is unanswered: the callee's 180 is
/// forwarded by the proxy and refused at the cut address, the worker's own
/// Timer A rungs leave and are refused at its address — every SIP entry
/// touching the address since the cut is undelivered, the caller sees
/// nothing, and the process is still up. Once restored, the worker's next
/// rung crosses and the call completes on it.
///
/// The recording pairs a send with the earliest later arrival of the same
/// bytes, so once a byte-identical rung crosses after the restore the refused
/// rungs before it can no longer be told apart by `delivered` alone: the
/// verdict on the cut window is read while it is still open, and the
/// verdict on the whole run is on `received_ms` — no datagram touching the
/// address is received inside the window.
#[tokio::test(start_paused = true)]
async fn cut_signalling_delivers_no_datagram_to_or_from_the_node() {
    let name = "fault-cut-signalling";
    let Cluster { mut fh, alice, bob, proxy, w_b1, w_b2 } = spawn_cluster(name).await;

    // ── the b-leg INVITE reaches the callee; the worker is then cut ──────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    let pri_sip = primary.sip_addr();
    fh.cut_signalling(&pri_ord, pri_sip);
    let cut_seq = last_marker_seq(&fh);
    let cut_at = fh.now_ms() as u64;

    // ── toward the cut node: the callee's 180; from it: its Timer A rungs ────
    uas.respond(180, "Ringing").await;
    fh.advance(Duration::from_secs(2)).await;
    let touching: Vec<sip_net::RecordedSipEntry> = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.seq > cut_seq && (e.from == pri_sip || e.to == pri_sip))
        .collect();
    let toward = touching.iter().filter(|e| e.to == pri_sip).count();
    let from = touching.iter().filter(|e| e.from == pri_sip).count();
    assert!(toward >= 1, "the proxy attempted a datagram toward the cut worker (the 180)");
    assert!(from >= 1, "the cut worker attempted a datagram of its own (an INVITE rung)");
    let crossed: Vec<&sip_net::RecordedSipEntry> =
        touching.iter().filter(|e| e.delivered).collect();
    assert!(
        crossed.is_empty(),
        "{} datagram(s) crossed the signalling cut at {pri_sip}; the first: {} → {}: {}",
        crossed.len(),
        crossed[0].from,
        crossed[0].to,
        String::from_utf8_lossy(&crossed[0].raw).lines().next().unwrap_or(""),
    );
    assert!(alice.take_queued().await.is_none(), "nothing reached the caller through the cut");
    assert!(primary.is_ready(), "the cut worker's process is still running");

    // ── restored: the worker's next rung crosses ─────────────────────────────
    fh.restore_signalling(&pri_ord, pri_sip);
    let restore_at = fh.now_ms() as u64;
    let crossed = fh
        .pump_until(Duration::from_millis(500), Duration::from_secs(10), async || {
            fh.sip_entries()
                .iter()
                .any(|e| e.from == pri_sip && e.received_ms.is_some_and(|at| at >= restore_at))
        })
        .await;
    assert!(crossed, "a datagram from {pri_sip} crosses once the cut is restored");
    let inside: Vec<sip_net::RecordedSipEntry> = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.from == pri_sip || e.to == pri_sip)
        .filter(|e| e.received_ms.is_some_and(|at| (cut_at..restore_at).contains(&at)))
        .collect();
    assert!(
        inside.is_empty(),
        "{} datagram(s) touching {pri_sip} were received inside the cut window \
         [{cut_at}, {restore_at}) ms; the first: {} → {}: {}",
        inside.len(),
        inside[0].from,
        inside[0].to,
        String::from_utf8_lossy(&inside[0].raw).lines().next().unwrap_or(""),
    );

    // ── the callee answers the one INVITE it holds; the call completes ───────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(backup, &pri_ord).await;
    assert!(primary.serves(&call_ref), "the restored worker serves the answered call");
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// **A crash stops every frame and datagram from the node.** After `crash()`
/// no frame is sent from any address the node owned and no datagram leaves
/// its SIP address; it serves nothing; the peer sees the node's stream close.
/// The caller's BYE fails over to the survivor, the node reboots and reclaims
/// the deferred terminal, and the call ends with one CDR.
#[tokio::test(start_paused = true)]
async fn crash_stops_every_frame_and_datagram_from_the_node() {
    let name = "fault-crash-stops-everything";
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = spawn_cluster(name).await;

    // ── an established, replicated call ──────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    assert!(primary.serves(&call_ref), "the primary serves the call");
    assert!(
        survivor.flow_caught_up(&pri_ord, Partition::Bak),
        "the primary's puller is connected to the survivor and caught up",
    );
    let owned = owned_by(&fh.repl_report(), &pri_ord);
    assert!(owned.len() >= 2, "the primary owns its lane and its pullers' locals: {owned:?}");
    let pri_sip = primary.sip_addr();

    // ── the crash ────────────────────────────────────────────────────────────
    fh.mark(&pri_ord, None, "crash", "the serving node dies");
    let crash_seq = last_marker_seq(&fh);
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_secs(2)).await;

    // ── the caller ends the call: the survivor takes it over ─────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(2)).await;

    // ── nothing left the dead node since the marker ──────────────────────────
    let report = fh.repl_report();
    let sent_by_dead: Vec<&CapturedFrame> = report
        .frames
        .iter()
        .filter(|f| f.seq > crash_seq && f.dir == Direction::Sent && owned.contains(&f.from))
        .collect();
    assert!(
        sent_by_dead.is_empty(),
        "the crashed {pri_ord} sent {} frame(s) after the crash; the first: {:?}",
        sent_by_dead.len(),
        sent_by_dead.first().map(|f| format!("{} → {}: {}", f.from, f.to, frame_summary(&f.frame))),
    );
    let sip_from_dead: Vec<sip_net::RecordedSipEntry> =
        fh.sip_entries().into_iter().filter(|e| e.seq > crash_seq && e.from == pri_sip).collect();
    assert!(
        sip_from_dead.is_empty(),
        "the crashed {pri_ord} sent {} datagram(s) after the crash; the first: {}",
        sip_from_dead.len(),
        String::from_utf8_lossy(&sip_from_dead[0].raw).lines().next().unwrap_or(""),
    );
    assert!(!primary.serves(&call_ref), "a crashed node serves nothing");
    assert!(!primary.is_ready(), "a crashed node is not ready");
    assert!(
        !survivor.flow_caught_up(&pri_ord, Partition::Bak),
        "the survivor saw the dead node's stream close",
    );

    // ── reboot and reclaim: the deferred terminal is discharged once ─────────
    fh.advance(Duration::from_secs(30)).await;
    reboot_and_ready(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let reboot_seq = last_marker_seq(&fh);
    let report = fh.repl_report();
    let pri_listen = listen_of(&report, &pri_ord);
    let reopened = report.frames.iter().any(|f| {
        f.seq > reboot_seq
            && f.dir == Direction::Sent
            && f.to == pri_listen
            && matches!(&f.frame, Frame::PullRequest { caller, .. } if caller != &pri_ord)
    });
    assert!(reopened, "the survivor's puller opened a fresh stream to the rebooted node");
    fh.advance(Duration::from_secs(10)).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// **A partition holds a replacement incarnation.** A replacement of the
/// serving ordinal comes up beside the withdrawn original on a fresh
/// replication listen address, reclaims the call, and the original dies. A
/// partition named on the ordinals then holds every stream between the
/// replacement and its peer: the re-INVITE the replacement serves is flushed
/// and zero frames cross until the heal.
#[tokio::test(start_paused = true)]
async fn partition_holds_a_replacement_incarnation() {
    let name = "fault-partition-holds-replacement";
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = spawn_cluster(name).await;

    // ── an established, replicated call ──────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let (elder, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    assert!(elder.serves(&call_ref), "the primary serves the call");

    // ── withdraw, replace, readmit; the elder dies once the replacement serves
    fh.withdraw(&pri_ord);
    fh.advance(Duration::from_secs(1)).await;
    let replacement = fh.spawn_replacement(&pri_ord).await;
    fh.readmit(&pri_ord, replacement.sip_addr());
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if replacement.is_ready() {
            break;
        }
    }
    assert!(replacement.is_ready(), "the replacement re-hydrated and became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(1)).await;
    fh.mark(&pri_ord, None, "crash", "the replaced incarnation is killed");
    elder.crash();
    let reclaimed = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(60), async || {
            replacement.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement reclaimed the call");
    let report = fh.repl_report();
    let rep_listen = listen_of(&report, &pri_ord);
    let declared = report
        .lanes
        .iter()
        .filter(|(_, ord)| ord.as_str() == pri_ord)
        .map(|(addr, _)| *addr)
        .min_by_key(|a| a.port())
        .expect("the ordinal's declared lane");
    assert_ne!(rep_listen, declared, "the replacement listens beside the declared address");
    let before_p = survivor.call_gen(BAK, &pri_ord, &call_ref).expect("the replica has a (p,b)");

    // ── the cut, then the replacement mutates: a re-INVITE is answered ──────
    fh.partition(&pri_ord, &bak_ord);
    let cut_seq = last_marker_seq(&fh);
    reinvite(&fh, &mut dialog, &bob).await;
    fh.advance(Duration::from_secs(2)).await;
    assert!(
        replacement.call_gen(PRI, &pri_ord, &call_ref).is_some_and(|p| p > before_p),
        "the replacement authored a new version of the call",
    );
    let report = fh.repl_report();
    let staged: Vec<&CapturedFrame> = report
        .frames
        .iter()
        .filter(|f| f.seq > cut_seq && f.dir == Direction::Sent && f.from == rep_listen)
        .filter(|f| is_data(&f.frame))
        .collect();
    assert!(!staged.is_empty(), "the replacement flushed the new version behind the cut");
    let crossed = received_between(&report, &pri_ord, &bak_ord, cut_seq);
    assert!(
        crossed.is_empty(),
        "the partition delivered {} frame(s) between the replacement {pri_ord} and {bak_ord}; \
         the first: {:?}",
        crossed.len(),
        crossed.first().map(|f| format!("{} → {}: {}", f.from, f.to, frame_summary(&f.frame))),
    );
    assert_eq!(
        survivor.call_gen(BAK, &pri_ord, &call_ref),
        Some(before_p),
        "the replica's version did not move across the cut",
    );

    // ── the heal: the replica converges ──────────────────────────────────────
    fh.heal(&pri_ord, &bak_ord);
    fh.advance(Duration::from_secs(2)).await;
    assert_eq!(
        survivor.call_gen(BAK, &pri_ord, &call_ref),
        replacement.call_gen(PRI, &pri_ord, &call_ref),
        "the replica converged on the heal",
    );

    // ── the call ends on the replacement, cleanly ────────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&replacement, survivor], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, replacement, proxy));
}
