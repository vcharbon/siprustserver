//! The failover fault matrix (ADR-0011 X10): faults beyond the canonical
//! scenario of `failover.rs`, each on one call through the proxy and two
//! replicating workers under one fake clock — a backup crash between two
//! Timer-A copies of one INVITE, a crash during re-hydration, a partition at
//! the failover, and a double fault.
//!
//! Beyond the failover sweep: a crash during re-hydration, a partition or a double fault at the
//! failover, and the proxy's repeat of a forward whose backup died.

use std::time::Duration;

use failover_harness::{
    assert_call_ended, worker_ordinals, FailoverHarness, ReplicatedB2buaSut, WorkerHealth,
};

use crate::failover::{find_backed_up_ref, reboot_and_reclaim};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// Matrix: the backup crashes between two Timer-A copies of one INVITE. The
/// callee is silent and the worker's 100 is lost on its way to the caller, so
/// both INVITE client transactions keep their Timer A: the caller's rung and
/// the primary's b-leg rung reach the proxy after the backup was killed. The
/// proxy repeats each forward — same target, same branch, and the cookie its
/// first copy carried (the §16.6 / §17.2.3 memo), the dead backup still named:
/// the taker keeps the first copy's route set (§12.1.1), so the dialog keeps
/// one cookie. The primary's server transaction absorbs the a-leg copy, the
/// callee the b-leg copy, the call completes on the survivor, and the trace is
/// clean under `rung-byte-identical`.
#[tokio::test(start_paused = true)]
async fn matrix_backup_crash_between_timer_a_copies_of_one_invite() {
    let mut fh = FailoverHarness::new("s10b-backup-crash-between-timer-a-copies", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;

    // alice INVITEs, cut off while the worker's 100 crosses; bob gets the b-leg.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut uas = bob.receive("INVITE").await;
    fh.restore_signalling("alice", alice.addr());
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    assert!(!bak_ord.is_empty(), "two live workers: the cookie names a backup");
    let primary: std::net::SocketAddr = if pri_ord == "b1" { B1 } else { B2 }.parse().unwrap();

    // The backup dies before either Timer A fires.
    fh.mark(&bak_ord, None, "crash", "between two Timer-A copies of the INVITE");
    if bak_ord == "b1" {
        w_b1.crash();
    } else {
        w_b2.crash();
    }
    proxy.set_health(&bak_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(500)).await; // Timer A (T1): the primary's b-leg rung

    // Timer A: the caller re-sends THE INVITE — the bytes that left, again.
    let invite = fh
        .sip_entries()
        .into_iter()
        .find(|e| e.from == alice.addr() && e.to == proxy.addr() && e.opens_with("INVITE "))
        .map(|e| e.raw)
        .expect("the caller's INVITE is on the wire");
    alice.try_send_datagram(&invite, proxy.addr()).await.unwrap();
    fh.advance(Duration::from_millis(100)).await;

    // The proxy repeated its forward to the SAME primary under the SAME branch
    // and the SAME cookie: the dead backup is still named.
    let forwarded: Vec<Vec<u8>> = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.from == proxy.addr() && e.to == primary && e.opens_with("INVITE "))
        .map(|e| e.raw)
        .collect();
    assert_eq!(forwarded.len(), 2, "two copies of the one INVITE forwarded to {pri_ord}");
    assert_eq!(
        sip_message::sniff::via_branch(&forwarded[0]),
        sip_message::sniff::via_branch(&forwarded[1]),
        "§16.11: the retransmission rides the branch the first copy got"
    );
    let cookie = |raw: &[u8]| {
        sip_message::sniff::header_values(raw, "Record-Route")
            .into_iter()
            .find(|row| row.contains("w_pri="))
            .expect("the proxy's cookie Record-Route")
    };
    assert!(
        cookie(&forwarded[0]).contains(&format!("w_bak={bak_ord}")),
        "{}",
        cookie(&forwarded[0])
    );
    assert_eq!(cookie(&forwarded[1]), cookie(&forwarded[0]), "the cookie repeats");
    // The b-leg rung went to bob too.
    let to_bob = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.from == proxy.addr() && e.to == bob.addr() && e.opens_with("INVITE "))
        .count();
    assert_eq!(to_bob, 2, "the primary's Timer A re-sent its b-leg INVITE through the proxy");

    // The primary absorbed the a-leg copy; bob absorbs the b-leg copy and answers.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dlg = call.ack().await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    scenario_harness::callflow::hangup(&mut dlg, &bob).await;
    fh.advance(Duration::from_millis(300)).await;
    // Released on the survivor: the primary left no per-call state behind.
    let survivor = if pri_ord == "b1" { &w_b1 } else { &w_b2 };
    assert!(
        survivor.memory_clean(),
        "{pri_ord} released the call ({} live, {} locks)",
        survivor.active_calls(),
        survivor.lock_count()
    );
    let cdrs = survivor.cdr_records();
    assert_eq!(cdrs.len(), 1, "the survivor wrote the call's one CDR");
    assert_call_ended(&[&w_b1, &w_b2], &cdrs[0].call_ref, 1, "the primary was never touched").await;
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

/// Matrix: crash during re-hydration — crash B1 AGAIN while it is bootstrapping
/// from B2 (before it reaches ready), then reboot it once more. It must retry /
/// re-bootstrap with no corruption and eventually reach ready.
#[tokio::test(start_paused = true)]
async fn matrix_crash_during_rehydration() {
    let mut fh = FailoverHarness::new("s10b-crash-during-rehydration", &["b1", "b2"]);
    let _alice = fh.agent("alice", ALICE).await;
    let _bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready());

    // Crash b1, reboot it (begins re-hydration), then crash it AGAIN immediately
    // (mid-bootstrap) before it reaches ready.
    fh.mark("b1", None, "crash", "first");
    w_b1.crash();
    proxy.set_health("b1", WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;

    w_b1.reboot().await;
    fh.mark("b1", None, "reboot", &format!("gen={}", w_b1.gen()));
    // Only a tiny advance — likely still bootstrapping — then crash again.
    fh.advance(Duration::from_millis(100)).await;
    fh.mark("b1", None, "crash", "during re-hydration");
    w_b1.crash();
    fh.advance(Duration::from_millis(200)).await;

    // Reboot once more and let it fully re-hydrate. No corruption: it reaches
    // ready again at an even higher gen.
    w_b1.reboot().await;
    fh.mark("b1", None, "reboot", &format!("gen={}", w_b1.gen()));
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if w_b1.is_ready() {
            break;
        }
    }
    assert!(w_b1.is_ready(), "b1 re-bootstrapped to ready after a crash mid-re-hydration");
    assert!(w_b1.gen() >= 3, "two reboots bumped the gen at least to 3, got {}", w_b1.gen());
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

/// Matrix: partition during failover — cut the repl link between B1 and B2 at the
/// failover moment, then heal. The acting-backup b2 still serves the in-dialog
/// request (its reverse-propagation to the down/cut primary is best-effort), and
/// after heal + B1 reboot the cluster re-converges (B1 reaches ready). The
/// partition holds every stream between the pair — the ones already open, which
/// the fabric attributes to their owning node, and any opened while the cut
/// lasts; B1 is crashed to make the failover happen, and the cut then keeps its
/// reconnect-on-reboot from landing until heal.
#[tokio::test(start_paused = true)]
async fn matrix_partition_during_failover() {
    let mut fh = FailoverHarness::new("s10b-partition-during-failover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    let bak_ord = b2.ordinal().to_string();
    let call_ref = find_backed_up_ref(b2, &primary_ord).await;

    // Failover moment: partition the pair (blocks reconnect) AND crash the
    // primary (closes its established streams). Mark it dead.
    fh.partition(&primary_ord, &bak_ord);
    fh.mark(&primary_ord, None, "crash", "at failover");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // The acting-backup b2 still handles the in-dialog BYE (reverse-propagation
    // to the partitioned/crashed primary is best-effort; the call is served).
    let b2_before = b2.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        b2.metrics().creations_total() > b2_before,
        "acting-backup served the in-dialog request despite the partition",
    );

    // Heal + reboot the primary → it reconnects and re-converges to ready.
    fh.heal(&primary_ord, &bak_ord);
    b1.reboot().await;
    fh.mark(&primary_ord, None, "reboot", &format!("gen={}", b1.gen()));
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "primary re-converged to ready after heal + reboot");
    fh.advance(Duration::from_secs(10)).await;
    assert_call_ended(&[&*b1, &*b2], &call_ref, 1, "the reclaim discharges the deferred end").await;
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

/// Matrix: double-fault — crash B1, then before recovery the BYE arrives on B2
/// AND B2 takes a transient fault (we cut B2→B1 reverse-propagation so its
/// best-effort reclaim push fails). Liveness must be preserved: B2 still serves
/// the in-dialog request and the harness stays live.
#[tokio::test(start_paused = true)]
async fn matrix_double_fault() {
    let mut fh = FailoverHarness::new("s10b-double-fault", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    let bak_ord = b2.ordinal().to_string();
    let call_ref = find_backed_up_ref(b2, &primary_ord).await;

    // Fault 1: crash the primary. Fault 2: partition the pair so b2's reverse
    // reclaim push to the (down) primary also cannot land — a transient repl
    // fault on the acting-backup's propagation path.
    fh.mark(&primary_ord, None, "crash", "fault 1");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.partition(&primary_ord, &bak_ord);
    fh.mark(&bak_ord, Some(&primary_ord), "partition", "fault 2 (reverse-prop cut)");
    fh.advance(Duration::from_millis(300)).await;

    // Liveness: the BYE still completes on b2 despite the double fault.
    let b2_before = b2.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        b2.metrics().creations_total() > b2_before,
        "double-fault: acting-backup still served the in-dialog request (liveness)",
    );

    // Both faults clear: the primary's reclaim discharges the deferred end.
    fh.heal(&primary_ord, &bak_ord);
    reboot_and_reclaim(&mut fh, &proxy, b1).await;
    assert_call_ended(&[&*b1, &*b2], &call_ref, 1, "the reclaim discharges the deferred end").await;
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}
