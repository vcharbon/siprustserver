//! **Every instant the call record holds keeps its anchor across any number of
//! takeovers** (ADR-0055 §9). The two workers' wall clocks disagree by a minute;
//! the instants a copy holds are read in its own node's frame, so the duration
//! cap and the creation time stay where the writer put them, within the
//! re-anchor deadband, however many hops the body made.
//!
//! ```text
//!   taken over twice: the backup's own write between the two takeovers
//!     INVITE → 200 → ACK on the primary · primary crashed in place
//!     alice INFO  → the backup takes the call over (copy 1), then sheds it
//!     alice INFO  → the backup takes the call over again (copy 2), sheds it
//!     alice BYE   → the backup ends the call · the primary reboots, discharges
//!
//!   reclaimed through the backup: the dead primary's body, never rewritten
//!     INVITE → 200 → ACK on the primary · primary crashed
//!     primary reboots, reclaims the body the backup held for it
//!     alice BYE   → served by the rebooted primary
//! ```

use std::time::Duration;

use call::{Call, TimerKey, TimerType};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// `b1`'s wall clock runs this far ahead of `b2`'s.
const B1_AHEAD_MS: i64 = 60_000;

/// The re-anchor deadband: what an instant may move by across a hop, the
/// replication transit included.
const DEADBAND_MS: i64 = 1_000;

/// `ordinal`'s wall-clock offset in this harness.
fn offset_of(ordinal: &str) -> i64 {
    if ordinal == "b1" {
        B1_AHEAD_MS
    } else {
        0
    }
}

/// The instants of `call` this file pins: its creation time and its duration
/// cap's deadline.
fn instants(call: &Call) -> (i64, i64) {
    let cap = TimerKey::call(TimerType::GlobalDuration);
    let cap_at = call.timers.iter().find(|t| t.key == cap).map(|t| t.fire_at);
    (call.created_at, cap_at.expect("the answered call holds its duration cap"))
}

/// `call`'s instants are `expected` moved by `shift`, within the deadband.
fn assert_anchored(call: &Call, expected: (i64, i64), shift: i64, what: &str) {
    let (created, cap) = instants(call);
    let created_delta = created - (expected.0 + shift);
    let cap_delta = cap - (expected.1 + shift);
    assert!(
        created_delta.abs() < DEADBAND_MS && cap_delta.abs() < DEADBAND_MS,
        "{what}: the creation time is off its anchor by {created_delta} ms and the duration \
         cap by {cap_delta} ms (each must be within {DEADBAND_MS} ms)",
    );
}

/// The proxy plus both workers, `b1` a minute ahead, ready at steady state.
async fn bring_up(fh: &mut FailoverHarness) -> (ProxySut, ReplicatedB2buaSut, ReplicatedB2buaSut) {
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");
    (proxy, w_b1, w_b2)
}

/// Establish alice ⇄ bob through the proxy; the caller's dialog and the
/// primary's ordinal.
async fn establish(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: &ProxySut,
) -> (Dialog, String) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    (dialog, pri_ord)
}

/// The primary and the backup, by the primary's ordinal.
fn split_mut<'a>(
    pri_ord: &str,
    w_b1: &'a mut ReplicatedB2buaSut,
    w_b2: &'a mut ReplicatedB2buaSut,
) -> (&'a mut ReplicatedB2buaSut, &'a mut ReplicatedB2buaSut) {
    if pri_ord == "b1" {
        (w_b1, w_b2)
    } else {
        (w_b2, w_b1)
    }
}

/// The first replicated ref in `bak:{primary}` on the backup.
async fn find_backed_up_ref(
    fh: &FailoverHarness,
    backup: &ReplicatedB2buaSut,
    primary: &str,
) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary).await {
            return rf;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the backup");
}

/// alice sends an INFO bob answers, through whichever node serves it.
async fn info_round(dialog: &mut Dialog, bob: &Agent) {
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
}

/// Wait until `node` no longer serves `call_ref`: its takeover copy shed once
/// the transactions it served cleared.
async fn until_shed(fh: &FailoverHarness, node: &ReplicatedB2buaSut, call_ref: &str) {
    let shed = fh
        .pump_until(Duration::from_millis(500), Duration::from_secs(60), async || {
            !node.serves(call_ref)
        })
        .await;
    assert!(shed, "the takeover copy sheds once its transactions clear");
}

/// Reboot the crashed primary empty at a higher gen + new pod IP, re-learn its
/// address, drive it ready and let the go-active reclaim run.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) {
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    backup.simulate_peer_added(primary_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}

/// The cluster post-condition: settled, exactly one CDR, no trace and clean
/// per-call memory on both nodes.
async fn settle_to_one_cdr(
    fh: &FailoverHarness,
    peers: &[&Agent],
    nodes: [&ReplicatedB2buaSut; 2],
    call_ref: &str,
) {
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(call_ref).await
                && !nodes[1].holds_any_trace(call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(peers, Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&nodes[..], call_ref).await;
}

/// A call taken over twice by the same backup, which wrote the call between
/// the two takeovers: each copy holds the primary's instants moved into the
/// backup's frame once, never once per takeover.
#[tokio::test(start_paused = true)]
async fn a_call_taken_over_twice_keeps_its_instants_anchored() {
    let mut fh = FailoverHarness::new("instants-taken-over-twice", &["b1", "b2"])
        .with_worker_clock_offset("b1", B1_AHEAD_MS);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let (primary, backup) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let bak_ord = backup.ordinal().to_string();
    let call_ref = find_backed_up_ref(&fh, backup, &pri_ord).await;
    let written = instants(&primary.live_call(&call_ref).expect("the primary serves the call"));
    let skew = offset_of(&bak_ord) - offset_of(&pri_ord);

    // ── the primary dies in place; the backup takes the call over ──────────
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    info_round(&mut dialog, &bob).await;
    let first = backup.live_call(&call_ref).expect("the backup serves the taken-over call");
    assert_anchored(&first, written, skew, "the first takeover copy");
    until_shed(&fh, backup, &call_ref).await;

    // ── the backup takes the call over again, from the body it wrote ───────
    info_round(&mut dialog, &bob).await;
    let second = backup.live_call(&call_ref).expect("the backup serves the call again");
    assert_anchored(&second, written, skew, "the second takeover copy");
    until_shed(&fh, backup, &call_ref).await;

    // ── alice hangs up on the backup; the primary returns and discharges ───
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, backup, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], [&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// A primary that reboots reclaims the body its backup held for it, written
/// by its dead incarnation and never rewritten: the reclaimed call holds the
/// instants the dead incarnation wrote, in its own frame.
#[tokio::test(start_paused = true)]
async fn a_call_reclaimed_through_its_backup_keeps_its_instants_anchored() {
    let mut fh = FailoverHarness::new("instants-reclaimed-through-backup", &["b1", "b2"])
        .with_worker_clock_offset("b1", B1_AHEAD_MS);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let (primary, backup) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let call_ref = find_backed_up_ref(&fh, backup, &pri_ord).await;
    let written = instants(&primary.live_call(&call_ref).expect("the primary serves the call"));

    // ── the primary dies and reboots; it reclaims the call ─────────────────
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;
    reboot_and_reclaim(&mut fh, primary, backup, &pri_ord, &proxy).await;
    let reclaimed = primary.live_call(&call_ref).expect("the rebooted primary reclaimed the call");
    assert_anchored(&reclaimed, written, 0, "the reclaimed call");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], [&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
