//! **A dialog-level retransmission turn is quiet** (ADR-0014 amendment): a
//! rung of this node's own un-ACKed 2xx ladder (RFC 3261 §13.3.1.4) and the
//! re-ACK of a repeated inbound 2xx (§13.2.2.4) change no fact a CDR, a peer
//! or a node restoring the call needs. Such a turn advances the live copy —
//! the armed rung moves on — and moves nothing else: no version-vector bump,
//! no flush. The backup keeps the last write that changed the call; a node
//! that restores the call from it restarts the ladder from that write, bounded
//! by the give-up deadline the replicated timer ledger carries (ADR-0032 X2).
//!
//! ```text
//!   INVITE → 200 (offer/answer done) · the caller holds her ACK
//!   +0.5 s, +1.5 s, +3.5 s   the primary's rungs: copies to the caller,
//!                            nothing on the replication stream, (p,b) still
//!   kill after rung k        the primary reboots empty and reclaims its call
//!                            from the answer's write: the ledger's past-due
//!                            rung fires at once, then T1-doubling from the
//!                            first rung — 1 s, 2 s — tag-identical copies
//!   +ack_timeout             the replicated give-up ends the session: ACK to
//!                            the callee, BYE both ways, one CDR
//! ```
//!
//! The node that restores a ladder is the reclaiming primary: an acting-backup
//! takeover copy is reactive and sheds itself the moment the transaction it
//! served clears, its timers with it (`router::release`), so a call nobody
//! signals into waits for its primary. The timing axis is the rung the kill
//! lands after: `k = 1` (the first rung only) and `k = 3` (the ladder into its
//! T2 stretch, so a resume "where it stood" would pace at 4 s and a restart is
//! unmistakable).
//!
//! Every cell drives the protocol between advances and reads nothing that
//! depends on in-order draining of a paused runtime: each assertion follows a
//! settled advance, and the replication stream is read as a whole after it.
//!
//! The single-node twin (`b2bua-harness/tests/retransmission_turn_and_message_cap.rs`)
//! pins the runaway-message cap's side of the same rule.

use std::net::SocketAddr;
use std::time::Duration;

use call::TerminationCause;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use repl_net::frame::{Frame, Op};
use repl_net::transport::Direction;
use scenario_harness::Agent;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipResponse;
use sip_message::{serialize, CustomParser, Method, SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The deployment's un-ACKed 2xx deadline: past the reboot-and-reclaim window,
/// so the restored ladder shows before the give-up ends the session. The
/// ladder itself still ceases at Timer L (32 s of its own schedule).
const ACK_TIMEOUT_SEC: i64 = 60;

/// When each rung of the 2xx ladder is due after the original: T1, doubling,
/// capped at T2 (RFC 3261 §13.3.1.4).
const RUNG_DUE_MS: [u64; 3] = [500, 1_500, 3_500];

/// The gaps after the first copy of a ladder restarted from the answer's own
/// write — the ladder armed at rung 1 — as RFC 3261 §13.3.1.4 paces them: the
/// second rung 2·T1 after the first, the third 4·T1 after the second.
const RESTART_GAPS_MS: [u64; 2] = [1_000, 2_000];

/// Scheduling slack on a measured gap: the paused clock's advance chunk.
const SLACK_MS: u64 = 200;

/// One paused-clock step: the transit hop, so nothing lands between steps.
const STEP: Duration = Duration::from_millis(100);

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

/// Two replicating workers behind the proxy, both ready, the 2xx deadline
/// pinned at [`ACK_TIMEOUT_SEC`] on both (a tune survives a reboot).
async fn spawn_cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_worker_tune(|c| {
        c.ack_timeout_sec = ACK_TIMEOUT_SEC;
    });
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

/// Every replication `Put` for `call_ref` that left `ordinal`'s listener —
/// one per flush of the call the peer pulled. The lane map names each node's
/// listen address, and a listener's frames are the ones it SENDS from there.
fn puts_sent_by(fh: &FailoverHarness, ordinal: &str, call_ref: &str) -> usize {
    let report = fh.repl_report();
    let listener = report
        .lanes
        .iter()
        .find(|(_, ord)| ord.as_str() == ordinal)
        .map(|(addr, _)| *addr)
        .expect("the node's replication lane is named");
    report
        .frames
        .iter()
        .filter(|f| f.dir == Direction::Sent && f.from == listener)
        .filter(
            |f| matches!(&f.frame, Frame::Data { op: Op::Put, call_ref: r, .. } if r == call_ref),
        )
        .count()
}

/// Every INVITE 2xx `from` put on the wire for the dialog of `call_id`, in
/// send order, as `(sent_ms, response)`.
fn invite_2xx_sent_by(
    fh: &FailoverHarness,
    from: SocketAddr,
    call_id: &str,
) -> Vec<(u64, SipResponse)> {
    let mut out: Vec<(u64, SipResponse)> = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.from == from)
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r))
                if r.status() == 200
                    && *r.cseq().method() == Method::Invite
                    && r.call_id().as_str() == call_id =>
            {
                Some((e.sent_ms, r))
            }
            _ => None,
        })
        .collect();
    out.sort_by_key(|(ms, _)| *ms);
    out
}

/// The gaps between consecutive copies (ms).
fn gaps(copies: &[(u64, SipResponse)]) -> Vec<u64> {
    copies.windows(2).map(|w| w[1].0 - w[0].0).collect()
}

/// Read everything queued at the two peers, as their UAs would between the
/// scenario's own steps: a keepalive OPTIONS is answered 200 toward the hop it
/// came from, a copy of the answer at the caller is read and left un-ACKed
/// (her deviation), anything else is a defect. The reclaim re-bases the
/// restored keepalive to probe the recovered peers early, so the probes land
/// inside the ladder's window.
async fn service_peers(alice: &Agent, bob: &Agent, hop: SocketAddr) {
    while let Some(msg) = alice.take_queued().await {
        match msg {
            SipMessage::Response(r) if r.status() == 200 => {}
            SipMessage::Request(req) if req.method() == Method::Options => {
                let ok = generate_response(&req, 200, "OK", &GenerateResponseOpts::default());
                alice.try_send_datagram(&serialize(&SipMessage::Response(ok)), hop).await.unwrap();
            }
            SipMessage::Response(r) => panic!("alice read an unexpected {} response", r.status()),
            SipMessage::Request(req) => panic!("alice read an unexpected {} request", req.method()),
        }
    }
    while let Some(mut t) = bob.try_receive_tolerating("OPTIONS", &[]).await {
        t.respond(200, "OK").await;
    }
}

/// Reboot the crashed primary EMPTY at a higher gen + new pod IP, re-learn its
/// address, drive it ready and let the go-active reclaim run — the call is
/// re-served from the last write the backup holds, its ledger restored.
/// Returns the rebooted node's SIP address.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) -> SocketAddr {
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    survivor.simulate_peer_added(primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
    new_addr
}

/// One run of the scenario: the kill lands after rung `k`.
async fn own_2xx_ladder_across_a_kill(name: &str, k: usize) {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = spawn_cluster(name).await;
    // The one knowingly-unmet obligation is alice's, and it is the subject: her
    // missing ACK is what keeps the ladder walking across the kill. The callee's
    // 2xx is acknowledged by the give-up before its BYE (§13.2.2.4).
    fh.allow_rfc_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice deliberately never ACKs — the ladder across a kill is under test",
    );

    // ── the call answers; alice holds her ACK ────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let b_leg_invite_cseq = uas.request().cseq().seq();
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = call.expect(200).await;
    let answered_at = fh.now_ms() as u64;
    let a_call_id = answer.call_id().as_str().to_string();

    // ── the answer's write reaches the backup ────────────────────────────────
    fh.advance(Duration::from_millis(300)).await;
    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let answered_p = primary
        .call_gen(PartitionRole::Primary, &pri_ord, &call_ref)
        .expect("the primary stores its own answered call");
    assert_eq!(
        survivor.call_gen(PartitionRole::Backup, &pri_ord, &call_ref),
        Some(answered_p),
        "the backup holds the answer's version",
    );
    let puts_at_answer = puts_sent_by(&fh, &pri_ord, &call_ref);

    // ── k rungs walk on the primary ──────────────────────────────────────────
    // Up to the k-th rung's arrival at alice: the rung leaves at its due
    // instant and lands one hop later.
    let walked_until = RUNG_DUE_MS[k - 1] + 200;
    fh.advance(Duration::from_millis(walked_until - 300)).await;
    let copies = alice.drain().await;
    assert_eq!(copies, k, "alice received the {k} rung(s) the primary walked");
    assert_eq!(
        primary.metrics().retransmits_total("final-2xx", "INVITE", Some(200)),
        k as u64,
        "the primary counted its {k} rung(s)",
    );
    assert_eq!(
        primary.call_gen(PartitionRole::Primary, &pri_ord, &call_ref),
        Some(answered_p),
        "a rung changes no replicated fact: the primary's (p,b) does not move across {k} rung(s)",
    );
    assert_eq!(
        puts_sent_by(&fh, &pri_ord, &call_ref) - puts_at_answer,
        0,
        "a rung is not a write: the primary flushed nothing for the call after the answer",
    );
    assert_eq!(
        primary.metrics().repl_quiet_turns_total("own-rung"),
        k as u64,
        "each rung was persisted as a quiet turn: {k} rungs against 0 flushes",
    );
    assert_eq!(
        survivor.call_gen(PartitionRole::Backup, &pri_ord, &call_ref),
        Some(answered_p),
        "the backup still holds the answer's version",
    );

    // ── the primary dies and comes back empty; the reclaim restores the call
    //    from the answer's write and restarts its ladder ───────────────────────
    fh.mark(&pri_ord, None, "crash", "primary down with the 2xx un-ACKed");
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    let reborn_addr = reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    // Three copies from the reclaimed copy: enough to read two gaps. A ladder
    // resumed where it stood paces at up to T2, so give it 25 s.
    let mut copies = invite_2xx_sent_by(&fh, reborn_addr, &a_call_id);
    for _ in 0..50 {
        if copies.len() >= 3 {
            break;
        }
        fh.advance(Duration::from_millis(500)).await;
        service_peers(&alice, &bob, proxy.addr()).await;
        copies = invite_2xx_sent_by(&fh, reborn_addr, &a_call_id);
    }
    assert!(
        copies.len() >= 3,
        "the reclaimed call's ladder put copies on the wire: {} so far",
        copies.len(),
    );
    let measured = gaps(&copies[..3]);
    for (i, (got, want)) in measured.iter().zip(RESTART_GAPS_MS).enumerate() {
        assert!(
            (want..=want + SLACK_MS).contains(got),
            "a restored ladder restarts from the last replicated write, the answer's own: copy {} \
             lands {got} ms after the one before it, not the {want} ms RFC 3261 §13.3.1.4 owes \
             from the first rung (T1, doubling); gaps {measured:?}",
            i + 2,
        );
    }

    // ── every copy is the answer, byte for byte ──────────────────────────────
    for (sent_ms, copy) in &copies {
        assert_eq!(copy.to().tag(), answer.to().tag(), "same To-tag at {sent_ms} ms");
        assert_eq!(copy.cseq().seq(), answer.cseq().seq(), "same CSeq at {sent_ms} ms");
        assert_eq!(copy.body(), answer.body(), "same body at {sent_ms} ms");
    }

    // ── the give-up fires at the deadline the ledger carried ─────────────────
    // `answered_at` is alice's receipt, one hop after the send the deadline
    // is measured from; the steps stop a full second short of it.
    let deadline = answered_at + ACK_TIMEOUT_SEC as u64 * 1000;
    while (fh.now_ms() as u64) < deadline - 1_500 {
        fh.advance(Duration::from_millis(500)).await;
        service_peers(&alice, &bob, proxy.addr()).await;
    }
    assert_eq!(
        primary.metrics().repeat_give_ups_total("ack-of-2xx"),
        0,
        "short of the deadline the ledger carried, the session stands",
    );
    fh.advance(Duration::from_millis(2_500)).await;
    assert_eq!(
        primary.metrics().repeat_give_ups_total("ack-of-2xx"),
        1,
        "the reclaimed call gives up at the deadline the ledger carried, not later",
    );
    // Discard alice's queued copies so the next request she reads is the
    // give-up BYE (the BYE client transaction retransmits, so one is still in
    // flight after the drain).
    alice.drain().await;
    let give_up_ack = bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    assert!(give_up_ack.request().body().is_empty(), "the give-up ACK owes no answer");
    assert_eq!(give_up_ack.request().cseq().seq(), b_leg_invite_cseq, "on the INVITE's CSeq");
    alice.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;

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
        "exactly one CDR for the call whose ladder crossed the kill",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// Killed after the first rung: a resume where the ladder stood would pace
/// 2 s, then 4 s.
#[tokio::test(start_paused = true)]
async fn a_reclaim_after_one_rung_restarts_the_2xx_ladder_from_the_answer() {
    own_2xx_ladder_across_a_kill("quiet-rung-reclaim-k1", 1).await;
}

/// Killed after the third rung: a resume where the ladder stood would pace
/// 4 s, then 4 s.
#[tokio::test(start_paused = true)]
async fn a_reclaim_after_three_rungs_restarts_the_2xx_ladder_from_the_answer() {
    own_2xx_ladder_across_a_kill("quiet-rung-reclaim-k3", 3).await;
}

/// The rungs of the 2xx ladder inside Timer L (64·T1 = 32 s, RFC 3261
/// §13.3.1.4): T1 doubling to T2 — 0.5, 1.5, 3.5, 7.5, 11.5, 15.5, 19.5, 23.5
/// and 27.5 s after the original arm the next rung; the tenth, at 31.5 s,
/// would arm one at 35.5 s, past the bound, and ceases the ladder instead.
const RUNGS_BEFORE_CEASE: u64 = 9;
const CEASING_RUNG_DUE_MS: u64 = 31_500;

/// The rung that ceases the ladder is a write: it removes the rung entry from
/// the replicated ledger, which a node restoring the call would otherwise
/// re-fire. Every rung before it is quiet; the ceasing one bumps `p` and
/// flushes once. The caller's late ACK then discharges the still-retained 2xx
/// and the call ends properly.
#[tokio::test(start_paused = true)]
async fn the_rung_that_ceases_the_2xx_ladder_is_a_write() {
    let Cluster { fh, alice, bob, proxy, w_b1, w_b2 } = spawn_cluster("quiet-rung-cease").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, survivor): (&ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // ── the answer's write reaches the backup ────────────────────────────────
    fh.advance(Duration::from_millis(300)).await;
    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let answered_p = primary
        .call_gen(PartitionRole::Primary, &pri_ord, &call_ref)
        .expect("the primary stores its own answered call");
    let puts_at_answer = puts_sent_by(&fh, &pri_ord, &call_ref);
    let rungs = || primary.metrics().retransmits_total("final-2xx", "INVITE", Some(200));

    // ── every rung that arms another is quiet ────────────────────────────────
    // Up to one hop past the ninth rung, short of the tenth.
    fh.advance(Duration::from_millis(CEASING_RUNG_DUE_MS - 500 - 300)).await;
    alice.drain().await;
    assert_eq!(rungs(), RUNGS_BEFORE_CEASE, "the rungs that arm another have all left");
    assert_eq!(
        primary.call_gen(PartitionRole::Primary, &pri_ord, &call_ref),
        Some(answered_p),
        "no rung that armed another moved (p,b)",
    );
    assert_eq!(
        puts_sent_by(&fh, &pri_ord, &call_ref) - puts_at_answer,
        0,
        "no rung that armed another flushed",
    );
    assert_eq!(
        primary.metrics().repl_quiet_turns_total("own-rung"),
        RUNGS_BEFORE_CEASE,
        "each of them persisted quietly",
    );

    // ── the ceasing rung leaves, and writes ──────────────────────────────────
    fh.advance(Duration::from_millis(1_000)).await;
    alice.drain().await;
    assert_eq!(rungs(), RUNGS_BEFORE_CEASE + 1, "the tenth rung left at Timer L's edge");
    assert_eq!(
        primary.metrics().repl_quiet_turns_total("own-rung"),
        RUNGS_BEFORE_CEASE,
        "the ceasing rung was not persisted quietly",
    );
    assert_eq!(
        primary.call_gen(PartitionRole::Primary, &pri_ord, &call_ref),
        Some(answered_p + 1),
        "the rung that ceases the ladder scrubs the rung entry from the ledger: a write, one bump",
    );
    assert_eq!(
        puts_sent_by(&fh, &pri_ord, &call_ref) - puts_at_answer,
        1,
        "the ceasing rung flushed once",
    );
    assert_eq!(
        survivor.call_gen(PartitionRole::Backup, &pri_ord, &call_ref),
        Some(answered_p + 1),
        "the backup holds the ceased ladder's write",
    );

    // ── alice's late ACK discharges the retained 2xx; the call ends ─────────
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(300)).await;
    assert_eq!(primary.metrics().repeat_give_ups_total("ack-of-2xx"), 0, "no give-up");
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// `max_messages_per_call` for the cap cell: the INVITE, the 180, the 200 and
/// the relayed ACK stand at 4; two repeated 2xx reach the cap, the third
/// crosses it.
const CAP: u64 = 6;

/// The repeated inbound 2xx that trips the per-call message cap is the
/// teardown, never quiet: the repeats under the cap draw their re-ACK and
/// move nothing; the one that crosses it begins the termination — a bump and
/// a flush on that turn — and the call ends under `MessageCap` with one CDR.
#[tokio::test(start_paused = true)]
async fn the_repeated_2xx_that_trips_the_cap_is_a_write() {
    let mut fh = FailoverHarness::new("quiet-re-ack-cap", &["b1", "b2"]).with_worker_tune(|c| {
        c.ack_timeout_sec = ACK_TIMEOUT_SEC;
        c.max_messages_per_call = CAP;
    });
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

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, survivor): (&ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;

    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let confirmed_p = primary
        .call_gen(PartitionRole::Primary, &pri_ord, &call_ref)
        .expect("the primary stores its own confirmed call");
    let puts_at_confirm = puts_sent_by(&fh, &pri_ord, &call_ref);

    // ── two repeats under the cap: re-ACKed, nothing moves ───────────────────
    for _ in 0..2 {
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        for _ in 0..8 {
            fh.advance(STEP).await;
        }
    }
    assert_eq!(bob.drain().await, 2, "two re-ACKs reached bob");
    assert_eq!(
        primary.call_gen(PartitionRole::Primary, &pri_ord, &call_ref),
        Some(confirmed_p),
        "the repeats under the cap moved nothing",
    );
    assert_eq!(puts_sent_by(&fh, &pri_ord, &call_ref) - puts_at_confirm, 0, "and flushed nothing");
    assert_eq!(primary.metrics().repl_quiet_turns_total("re-ack"), 2, "two quiet re-ACKs");
    assert_eq!(primary.metrics().message_cap_terminated_total(), 0, "under the cap");

    // ── the third crosses the cap: the turn is the teardown ──────────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    for _ in 0..8 {
        fh.advance(STEP).await;
    }
    assert_eq!(primary.metrics().message_cap_terminated_total(), 1, "the cap tripped");
    assert_eq!(
        primary.metrics().repl_quiet_turns_total("re-ack"),
        2,
        "the tripping turn was not quiet"
    );
    assert_eq!(
        primary.call_gen(PartitionRole::Primary, &pri_ord, &call_ref),
        Some(confirmed_p + 1),
        "the turn that trips the cap begins the termination: a write, one bump",
    );
    assert_eq!(
        puts_sent_by(&fh, &pri_ord, &call_ref) - puts_at_confirm,
        1,
        "the tripping turn flushed the terminating body once",
    );

    // ── the teardown is a proper one: BYE both ways, answered ────────────────
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive_tolerating("BYE", &["ACK"]).await.respond(200, "OK").await;
    drop(dialog);
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    let cause = [&w_b1, &w_b2]
        .iter()
        .flat_map(|n| n.cdr_records())
        .find(|r| r.call_ref == call_ref)
        .and_then(|r| r.termination.map(|t| t.cause));
    assert_eq!(cause, Some(TerminationCause::MessageCap), "the CDR names the cap as the cause");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The re-ACK of a repeated inbound 2xx (RFC 3261 §13.2.2.4) re-sends the
/// retained ACK and nothing else: three repeats draw three re-ACKs, and the
/// primary's (p,b) and replication stream do not move for any of them.
#[tokio::test(start_paused = true)]
async fn a_re_ack_of_a_repeated_2xx_replicates_nothing() {
    let Cluster { fh, alice, bob, proxy, w_b1, w_b2 } = spawn_cluster("quiet-re-ack").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, survivor): (&ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── the confirmed call's write reaches the backup ────────────────────────
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let confirmed_p = primary
        .call_gen(PartitionRole::Primary, &pri_ord, &call_ref)
        .expect("the primary stores its own confirmed call");
    let puts_at_confirm = puts_sent_by(&fh, &pri_ord, &call_ref);

    // ── bob repeats his 2xx three times, as a callee whose ACK was lost does ──
    // The script took the ACK first so the b-leg holds the branch the re-ACK
    // must reuse; the peer-side deviation is that bob repeats anyway.
    const REPEATS: usize = 3;
    let mut re_acks = 0;
    for _ in 0..REPEATS {
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        for _ in 0..8 {
            fh.advance(STEP).await;
        }
        re_acks += bob.drain().await;
    }
    assert_eq!(re_acks, REPEATS, "RFC 3261 §13.2.2.4: every repeated 2xx drew its re-ACK");
    assert_eq!(
        primary.metrics().retransmits_total("trigger", "ACK", None),
        REPEATS as u64,
        "each re-ACK is counted once as a trigger repeat",
    );
    assert_eq!(
        primary.call_gen(PartitionRole::Primary, &pri_ord, &call_ref),
        Some(confirmed_p),
        "a re-ACK changes no replicated fact: the primary's (p,b) does not move across \
         {REPEATS} re-ACKs",
    );
    assert_eq!(
        puts_sent_by(&fh, &pri_ord, &call_ref) - puts_at_confirm,
        0,
        "a re-ACK is not a write: the primary flushed nothing for the call after the ACK",
    );
    assert_eq!(
        primary.metrics().repl_quiet_turns_total("re-ack"),
        REPEATS as u64,
        "each re-ACK was persisted as a quiet turn: {REPEATS} re-ACKs against 0 flushes",
    );

    // ── the call ends properly on the live primary ───────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
