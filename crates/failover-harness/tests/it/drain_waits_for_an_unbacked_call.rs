//! **A drain never reads an unbacked call as held, and ends it off the wire at
//! its grace** (ADR-0055 §3, §5, ADR-0031 D2). The call's backup refused its
//! replica at its ceiling, so the call has no backup for life and its
//! backup's flow reads caught up all the same. The caught-up exit is an exit
//! on evidence that a peer holds every live call: with an unbacked call live,
//! there is none, and the drain waits for the call or the grace, as a non-HA
//! B2BUA would. At the grace the drain ends the established call where it
//! stands: its CDR is written and nothing is sent. The parties' later BYEs
//! reach the backup, which holds no copy and answers `481`.
//!
//! ```text
//!   established · the backup refuses the replica · the flow reads caught up
//!   t0         the primary is withdrawn from routing and drains
//!   t0..grace  no caught-up exit while the unbacked call lives
//!   grace      the primary ends the call: one CDR, no SIP, nothing resident
//!   then       the primary exits; alice ─BYE─▶ backup ─▶ 481; bob likewise
//! ```
//!
//! Beyond the failover sweep: a drain never reads an unbacked call as held.

use std::time::Duration;

use b2bua::capacity::BackupCeilings;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, DrainBounds, DrainExit,
    FailoverHarness, Partition, ReplicatedB2buaSut,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const GRACE: Duration = Duration::from_secs(5);
const BOUNDS: DrainBounds = DrainBounds {
    grace: GRACE,
    floor: Duration::from_millis(1000),
    release_flush: Duration::from_secs(3),
    cdr_flush: Duration::ZERO,
};

#[tokio::test(start_paused = true)]
async fn a_drain_waits_for_an_unbacked_call_then_ends_it_off_the_wire() {
    let replica = b2bua::ReplicaConfig {
        backup: BackupCeilings { calls: Some(0), rss_bytes: None },
        ..Default::default()
    };
    let name = "drain-waits-for-an-unbacked-call";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_worker_replica(replica);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let limiter = crate::away_backup_support::Limiter::serve().await;
    let mut w_b1 = limiter.worker(&mut fh, "b1", B1, &["b2"]).await;
    let mut w_b2 = limiter.worker(&mut fh, "b2", B2, &["b1"]).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── an established call whose backup refused its replica ─────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut bob_dialog = uas.dialog();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    let call_ref = primary.scan_primary(&pri_ord).pop().expect("the primary stores the call");
    assert!(backup.scan_backed_up(&pri_ord).is_empty(), "the backup refused the replica");
    assert!(primary.flow_caught_up(&bak_ord, Partition::Bak), "the backup's flow reads caught up");

    // ── the endpoint is withdrawn; the drain finds no peer holding the call ───
    let t0 = fh.now_ms();
    fh.withdraw_routing(&pri_ord);
    let mut drain = fh.begin_drain_pending(primary, BOUNDS);
    while fh.now_ms() - t0 < GRACE.as_millis() as i64 {
        let early = drain.poll();
        assert!(early.is_none(), "no exit while the unbacked call lives, got {early:?}");
        fh.advance(Duration::from_millis(100)).await;
    }
    let mut outcome = None;
    for _ in 0..20 {
        if let Some(o) = drain.poll() {
            outcome = Some(o);
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    drop(drain);
    let outcome = outcome.expect("the drain returned");
    assert_eq!(outcome.exit, DrainExit::GracePeersBehind, "an unbacked call is never held");
    assert_eq!(outcome.residual, 1, "the call no peer held is the residual");
    assert_eq!(primary.metrics().drain_exits("caught_up"), 0);

    // ── at the grace the primary ended the call off the wire ────────────────
    assert_eq!(
        primary.cdr_records().iter().filter(|r| r.call_ref == call_ref).count(),
        1,
        "the draining primary wrote the call's CDR at the grace"
    );
    assert!(!primary.serves(&call_ref), "the call is released on the draining primary");
    assert!(primary.memory_clean(), "nothing of the call stays resident");
    assert_eq!(limiter.held(), 0, "the call's limiter set is released at the grace");
    let sent = fh
        .sip_entries()
        .iter()
        .filter(|e| e.from == primary.sip_addr() && e.sent_ms >= t0 as u64)
        .count();
    assert_eq!(sent, 0, "the draining primary sent no SIP to end the call");

    // ── the primary exits; the parties' BYEs reach the backup: 481 ──────────
    fh.depart(&pri_ord);
    fh.mark(&pri_ord, None, "crash", "the drained process exits");
    primary.crash();
    fh.advance(Duration::from_millis(300)).await;
    let mut bye = dialog.bye().await;
    bye.expect(481).await;
    let mut bye = bob_dialog.bye().await;
    bye.expect(481).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
