//! **A timer keeps its key and generation across a takeover or a reclaim.**
//! The ledger rides the replicated body: the backup serving the call after its
//! primary died, or the primary reclaiming it after its reboot, re-arms each
//! timer under the key and generation the last write carried, and a fire of
//! an instance that ledger no longer holds — one armed before a later re-arm
//! replaced it — reaches no rule there.
//!
//! ```text
//!   INVITE → 180 → 200 → ACK   on the primary: the duration cap armed at
//!                              setup, re-armed at the answer, replicated
//!   primary crashes            the proxy routes in-dialog traffic to the backup
//!   alice INFO → bob 200       the backup takes the call over
//!   setup cap's fire posted    stale on the copy: the call stays up
//!   alice BYE                  served on the backup, which discharges it
//!   primary reboots            nothing left to reclaim: one CDR
//! ```

use b2bua::store::envelope;
use std::time::Duration;

use b2bua::CallEvent;
use call::{Call, MsgpackCodec, TimerKey, TimerType};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

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

/// The generation `call`'s ledger holds the call's duration cap under.
fn cap_generation(call: &Call) -> Option<u64> {
    let cap = TimerKey::call(TimerType::GlobalDuration);
    call.timers.iter().find(|t| t.key == cap).map(|t| t.generation)
}

/// Reboot the crashed primary EMPTY at a higher gen + new pod IP, re-learn its
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

#[tokio::test(start_paused = true)]
async fn a_stale_fire_across_a_takeover_is_dropped() {
    let mut fh = FailoverHarness::new("timer-generation-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── the call rings, then answers, on the primary ────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, _) = worker_ordinals(uas.request());
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let call_ref = find_backed_up_ref(&fh, backup, &primary_ord).await;
    let ringing = primary.live_call(&call_ref).expect("the primary serves the call");
    let setup_cap = cap_generation(&ringing).expect("the setup arms the duration cap");

    fh.advance(Duration::from_secs(1)).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let replica = envelope::decode(
        &MsgpackCodec::new(),
        &backup
            .get(PartitionRole::Backup, &primary_ord, &call_ref)
            .await
            .expect("the backup holds the replica"),
    )
    .expect("the replica decodes")
    .call;
    let answer_cap = cap_generation(&replica).expect("the answer's cap rides the replica");
    assert!(answer_cap > setup_cap, "the answer re-armed the cap under a later generation");

    // ── the primary dies; the backup serves the caller's INFO ───────────────
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;

    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;

    let taken = backup.live_call(&call_ref).expect("the backup serves the taken-over call");
    assert_eq!(cap_generation(&taken), Some(answer_cap), "the copy re-armed the cap as replicated");
    for entry in replica.timers.iter().filter(|t| *t.timer_type() != TimerType::KeepaliveTimeout) {
        let held = taken.timers.iter().find(|t| t.key == entry.key);
        assert!(
            held.is_some_and(|t| t.generation == entry.generation),
            "the copy holds {:?} at generation {}: {:?}",
            entry.key,
            entry.generation,
            taken.timers,
        );
    }
    assert!(taken.timer_generation >= replica.timer_generation, "the generation counter rides");

    // ── the setup cap's fire reaches the copy: stale, it ends nothing ───────
    backup.post_event(CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: setup_cap,
        call_ref: call_ref.clone(),
        incarnation: Some(taken.incarnation().to_string()),
    });
    fh.advance(Duration::from_secs(1)).await;
    assert!(bob.try_receive_tolerating("BYE", &[]).await.is_none(), "bob is not hung up");
    assert!(alice.try_receive_tolerating("BYE", &[]).await.is_none(), "alice is not hung up");
    let after = backup.live_call(&call_ref).expect("the copy still serves the call");
    assert_eq!(cap_generation(&after), Some(answer_cap), "the live cap is untouched");

    // ── the call ends on the backup, which discharges it; the primary reboots ──
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;

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

/// The primary crashes with the answered call and reboots before any traffic
/// moves it: it reclaims the call from its backup, re-arming the ledger as
/// replicated, and the setup cap's fire reaching it is stale.
#[tokio::test(start_paused = true)]
async fn a_stale_fire_across_a_reclaim_is_dropped() {
    let mut fh = FailoverHarness::new("timer-generation-reclaim", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, _) = worker_ordinals(uas.request());
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let call_ref = find_backed_up_ref(&fh, backup, &primary_ord).await;
    let ringing = primary.live_call(&call_ref).expect("the primary serves the call");
    let setup_cap = cap_generation(&ringing).expect("the setup arms the duration cap");

    fh.advance(Duration::from_secs(1)).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let replica = envelope::decode(
        &MsgpackCodec::new(),
        &backup
            .get(PartitionRole::Backup, &primary_ord, &call_ref)
            .await
            .expect("the backup holds the replica"),
    )
    .expect("the replica decodes")
    .call;
    let answer_cap = cap_generation(&replica).expect("the answer's cap rides the replica");
    assert!(answer_cap > setup_cap, "the answer re-armed the cap under a later generation");

    // ── the primary dies and reboots; it reclaims the call ──────────────────
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;
    reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    let reclaimed = primary.live_call(&call_ref).expect("the rebooted primary reclaimed the call");
    assert_eq!(cap_generation(&reclaimed), Some(answer_cap), "the cap re-armed as replicated");

    // ── the setup cap's fire reaches the reclaimed call: stale ──────────────
    primary.post_event(CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: setup_cap,
        call_ref: call_ref.clone(),
        incarnation: Some(reclaimed.incarnation().to_string()),
    });
    fh.advance(Duration::from_secs(1)).await;
    assert!(bob.try_receive_tolerating("BYE", &[]).await.is_none(), "bob is not hung up");
    assert!(alice.try_receive_tolerating("BYE", &[]).await.is_none(), "alice is not hung up");
    let after = primary.live_call(&call_ref).expect("the primary still serves the call");
    assert_eq!(cap_generation(&after), Some(answer_cap), "the live cap is untouched");

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
