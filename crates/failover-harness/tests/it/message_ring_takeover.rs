//! **The message ring's turn numbers survive a takeover.** The ring and the
//! call's turn counter ride the replicated body: an acting backup serving the
//! call after its primary died numbers its turns from the counter the last
//! write carried — past every entry the body holds, never from `1` again — so
//! a reader grouping the ring by turn never merges a takeover turn with one
//! the primary handled.
//!
//! ```text
//!   INVITE → 180 → 200 → ACK   on the primary, replicated to the backup
//!   primary crashes            the proxy routes in-dialog traffic to the backup
//!   alice INFO → bob 200       the backup takes the call over: two turns
//!   alice BYE                  served on the backup, deferred terminal
//!   primary reboots            reclaims the deferred terminal: one CDR
//! ```

use std::time::Duration;

use b2bua::config::CdrConfig;
use call::{Call, CallBodyCodec, MessageDirection, MsgpackCodec};
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

/// Every ring entry of the call, with its leg, in `seq` order.
fn entries(call: &Call) -> Vec<(&str, &call::MessageEntry)> {
    let mut all: Vec<(&str, &call::MessageEntry)> = std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|l| l.messages.entries.iter().map(move |e| (l.leg_id.as_str(), e)))
        .collect();
    all.sort_by_key(|(_, e)| e.seq);
    all
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
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
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
async fn a_takeover_numbers_its_turns_past_the_replicated_ring() {
    let mut fh = FailoverHarness::new("message-ring-takeover-turns", &["b1", "b2"])
        .with_worker_tune(|c| {
            c.cdr = CdrConfig { message_ring: 32, captured_headers: Vec::new() };
        });
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

    // ── the call establishes on the primary and replicates ──────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, _) = worker_ordinals(uas.request());
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(&fh, backup, &primary_ord).await;
    let served = primary.live_call(&call_ref).expect("the primary serves the call");
    let replica = MsgpackCodec::new()
        .decode(
            &backup
                .get(PartitionRole::Backup, &primary_ord, &call_ref)
                .await
                .expect("the backup holds the replica"),
        )
        .expect("the replica decodes");
    assert_eq!(replica.message_turn, served.message_turn, "the counter rides the replica");
    assert_eq!(entries(&replica).len(), entries(&served).len(), "the ring rides the replica");
    let recorded = entries(&replica);
    assert!(!recorded.is_empty(), "the establishment is on the ring");
    assert!(
        recorded.iter().all(|(_, e)| e.turn < replica.message_turn),
        "every replicated entry is numbered below the counter: {recorded:?}"
    );

    // ── the primary dies; the backup serves the caller's INFO ───────────────
    fh.mark(&primary_ord, None, "crash", "primary down");
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;

    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;

    let taken = backup.live_call(&call_ref).expect("the backup serves the taken-over call");
    let after: Vec<_> = entries(&taken)
        .into_iter()
        .filter(|(_, e)| e.method == "INFO")
        .map(|(leg, e)| (leg, e.direction, e.code, e.turn))
        .collect();
    let next = replica.message_turn;
    let b_leg = taken.b_legs[0].leg_id.as_str();
    assert_eq!(
        after,
        vec![
            ("a", MessageDirection::Received, None, next),
            (b_leg, MessageDirection::Relayed, None, next),
            (b_leg, MessageDirection::Received, Some(200), next + 1),
            ("a", MessageDirection::Relayed, Some(200), next + 1),
        ],
        "the takeover's two turns continue the replicated count"
    );

    // ── the call ends on the backup; the rebooted primary discharges it ─────
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
