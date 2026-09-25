//! **A backup replica is not stored above the backup RSS ceiling, and the
//! next write after the reading falls back stores it** (ADR-0037). The
//! ceiling is read from each worker's injected system probe through the live
//! supervisor → puller wiring; the call itself is never touched by it.
//!
//! ```text
//!   call A establishes            its replica lands on its backup
//!   both workers' RSS → 2 000     above the 1 000 backup ceiling
//!   call B establishes            its backup does not store it; A's stays
//!   both workers' RSS → 0
//!   alice INFO on call B          the flush that follows stores B's replica
//!   both calls end                one CDR each, nothing left behind
//! ```

use std::time::Duration;

use b2bua::capacity::BackupBound;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut,
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

/// A confirmed call through the proxy; returns alice's dialog and the
/// primary's ordinal.
async fn establish(alice: &Agent, bob: &Agent, proxy: std::net::SocketAddr) -> (Dialog, String) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(proxy).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary, _) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    (dialog, primary)
}

/// `(primary, backup)` by the primary's ordinal.
fn roles<'a>(
    primary: &str,
    b1: &'a ReplicatedB2buaSut,
    b2: &'a ReplicatedB2buaSut,
) -> (&'a ReplicatedB2buaSut, &'a ReplicatedB2buaSut) {
    if primary == "b1" {
        (b1, b2)
    } else {
        (b2, b1)
    }
}

#[tokio::test(start_paused = true)]
async fn a_backup_above_its_rss_ceiling_is_stored_by_the_next_write_below_it() {
    let mut fh = FailoverHarness::new("backup-capacity-rss", &["b1", "b2"]).with_worker_tune(|c| {
        c.capacity.backup_rss_bytes = Some(1_000);
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

    // ── call A replicates under the ceiling ──────────────────────────────────
    let (mut dialog_a, primary_a) = establish(&alice, &bob, proxy.addr()).await;
    fh.advance(Duration::from_millis(500)).await;
    let (serving_a, backup_a) = roles(&primary_a, &w_b1, &w_b2);
    let ref_a = serving_a.scan_primary(&primary_a).pop().expect("the primary stores call A");
    assert!(backup_a.scan_backed_up(&primary_a).contains(&ref_a), "A is backed up");

    // ── above the ceiling, call B's replica is not stored ───────────────────
    w_b1.system().set_rss_bytes(Some(2_000));
    w_b2.system().set_rss_bytes(Some(2_000));
    fh.advance(Duration::from_millis(300)).await;
    let (mut dialog_b, primary_b) = establish(&alice, &bob, proxy.addr()).await;
    fh.advance(Duration::from_millis(500)).await;
    let (serving_b, backup_b) = roles(&primary_b, &w_b1, &w_b2);
    let ref_b = serving_b
        .scan_primary(&primary_b)
        .into_iter()
        .find(|r| *r != ref_a)
        .expect("the primary stores call B");
    assert!(!backup_b.scan_backed_up(&primary_b).contains(&ref_b), "B is not backed up");
    assert!(backup_b.capacity().backup_shed_total(BackupBound::Rss) >= 1);
    assert!(backup_a.scan_backed_up(&primary_a).contains(&ref_a), "A's replica stays");

    // ── below the ceiling, the next write stores it ─────────────────────────
    w_b1.system().set_rss_bytes(Some(0));
    w_b2.system().set_rss_bytes(Some(0));
    fh.advance(Duration::from_millis(300)).await;
    let mut info = dialog_b.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(backup_b.scan_backed_up(&primary_b).contains(&ref_b), "B is backed up now");

    // ── both calls end cleanly ──────────────────────────────────────────────
    scenario_harness::callflow::hangup(&mut dialog_a, &bob).await;
    scenario_harness::callflow::hangup(&mut dialog_b, &bob).await;
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&ref_a).await
                && !w_b2.holds_any_trace(&ref_b).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    for r in [&ref_a, &ref_b] {
        assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], r), 1, "exactly one CDR for {r}");
        assert_call_fully_released(&[&w_b1, &w_b2], r).await;
    }
    drop(proxy);
}
