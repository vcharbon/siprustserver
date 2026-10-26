//! **A call its backup discharged, still live at a node of its primary's
//! ordinal.** A backup's takeover copy that ends the call of a departed primary
//! discharges it — CDR, limiter release, delete (ADR-0047 D8). A node of that
//! ordinal can still hold the call live: the departed process itself, withdrawn
//! from the cluster while it keeps running, or a replacement that reclaimed the
//! call before the backup saw it rejoin. The copy's delete reaches that node's
//! store on its reclaim stream; the node then drops its live copy, writes no
//! CDR and sends nothing, so the call ends with the copy's one CDR.
//!
//! ```text
//!   established · P serves · B synchronized
//!   (running)  P withdrawn, process up     (rejoin)  P killed and departed;
//!                                                    P' reclaims the call, B
//!                                                    not told P' joined
//!   alice ──BYE──▶ proxy ──▶ B   (takeover copy: ends the call, discharges it)
//!   B's delete ──▶ P / P'        (the live copy there is dropped)
//! ```

use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut,
};
use scenario_harness::{Agent, Dialog};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The cluster with one established, replicated call.
pub(crate) struct Established {
    pub(crate) fh: FailoverHarness,
    pub(crate) alice: Agent,
    pub(crate) bob: Agent,
    pub(crate) proxy: failover_harness::ProxySut,
    pub(crate) w_b1: ReplicatedB2buaSut,
    pub(crate) w_b2: ReplicatedB2buaSut,
    pub(crate) pri_ord: String,
    pub(crate) call_ref: String,
    pub(crate) dialog: Dialog,
}

pub(crate) async fn establish(name: &str) -> Established {
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

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref =
        backup.scan_one_backed_up(&pri_ord).await.expect("the call replicated to the backup");
    Established { fh, alice, bob, proxy, w_b1, w_b2, pri_ord, call_ref, dialog }
}

/// Hang up onto the backup and settle: every node in `nodes` clean, the CDRs
/// they wrote for the call returned.
pub(crate) async fn hang_up_and_settle(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    dialog: &mut Dialog,
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
) -> (bool, usize) {
    scenario_harness::callflow::hangup(dialog, bob).await;
    let drained = fh
        .settle_terminal(async || {
            let mut clean = true;
            for n in nodes {
                clean &= n.memory_clean() && !n.holds_any_trace(call_ref).await;
            }
            clean
        })
        .await;
    fh.linger_peers(&[alice, bob], Duration::from_secs(3)).await;
    (drained, total_cdrs_for(nodes, call_ref))
}

/// The primary is withdrawn while its process keeps running (ADR-0031 case
/// 3b): the backup ends and discharges the call, and the running primary drops
/// its live copy on the backup's delete.
#[tokio::test(start_paused = true)]
async fn a_running_primary_drops_the_call_its_backup_discharged() {
    let Established { mut fh, alice, bob, proxy, w_b1, w_b2, pri_ord, call_ref, mut dialog } =
        establish("discharged-call-dropped-by-a-running-primary").await;
    fh.withdraw(&pri_ord);
    fh.advance(Duration::from_secs(1)).await;
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    assert!(primary.serves(&call_ref), "the withdrawn primary's process still serves the call");

    let nodes = [primary, backup];
    let (drained, cdrs) =
        hang_up_and_settle(&fh, &alice, &bob, &mut dialog, &nodes, &call_ref).await;
    assert_eq!(cdrs, 1, "the backup's discharge is the call's one CDR");
    assert!(drained, "the running primary dropped its live copy");
    assert_call_fully_released(&nodes, &call_ref).await;
    drop(proxy);
}

/// The primary is killed and departs; a replacement of its ordinal reclaims
/// the call from the backup before the backup is told the ordinal rejoined.
/// The backup ends and discharges the call; the replacement drops the copy it
/// reclaimed on the backup's delete.
#[tokio::test(start_paused = true)]
async fn a_replacement_drops_the_call_its_backup_discharged_before_seeing_it_rejoin() {
    let Established {
        mut fh,
        alice,
        bob,
        proxy,
        mut w_b1,
        mut w_b2,
        pri_ord,
        call_ref,
        mut dialog,
    } = establish("discharged-call-dropped-by-a-replacement").await;
    {
        let elder = if pri_ord == "b1" { &mut w_b1 } else { &mut w_b2 };
        fh.mark(&pri_ord, None, "crash", "the primary is killed");
        elder.crash();
    }
    fh.depart(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;
    let replacement = fh.spawn_replacement(&pri_ord).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            replacement.is_ready() && replacement.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement reclaimed the call");

    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let nodes = [&replacement, backup];
    let (drained, cdrs) =
        hang_up_and_settle(&fh, &alice, &bob, &mut dialog, &nodes, &call_ref).await;
    assert_eq!(cdrs, 1, "the backup's discharge is the call's one CDR");
    assert!(drained, "the replacement dropped the copy it reclaimed");
    assert_call_fully_released(&nodes, &call_ref).await;
    drop(proxy);
}
