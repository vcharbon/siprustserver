//! **A replication stream that goes silent mid-call is reopened; calls never
//! notice.**
//!
//! The primary's stream to its backup stops delivering without closing (a
//! peer gone silent, no reset). The backup's pullers leave it after 2 s with
//! nothing received and pull again from their watermark, through the ordinary
//! backoff; the SIP plane never depends on it.
//!
//! ```text
//!   call 1 answered · primary → backup replication held (no close)
//!   ≤ 2 s + backoff    the backup re-pulls the primary from its watermark
//!   during the hold    call 2 is set up, answered and hung up
//!   20 s               call 1 hung up
//!   25 s               the hold is lifted: the backup catches up
//!   then               both calls released on both nodes, one CDR each
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut,
};
use repl_net::frame::{Frame, Watermark};
use repl_net_testkit::Direction;
use scenario_harness::Agent;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The puller's idle bound and the fast tier's first backoff step.
const IDLE_MS: i64 = 2_000;
const BACKOFF_FLOOR_MS: i64 = 100;
/// The server's `Noop` interval: each stream carried a frame at most this long
/// before the hold.
const NOOP_MS: i64 = 500;

fn report_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports")
        .join("silent-replication-stream")
}

/// The first replicated ref in `bak:{primary}` on `backup`.
async fn backed_up_ref(fh: &FailoverHarness, backup: &ReplicatedB2buaSut, primary: &str) -> String {
    let found = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(2), async || {
            backup.scan_one_backed_up(primary).await.is_some()
        })
        .await;
    assert!(found, "the backup holds the call's replica");
    backup.scan_one_backed_up(primary).await.unwrap()
}

/// Every `PullRequest` `caller` sent at or after `from_ms`, with its time and
/// watermark.
fn pulls_by(fh: &FailoverHarness, caller: &str, from_ms: i64) -> Vec<(i64, Watermark)> {
    fh.repl_report()
        .frames
        .iter()
        .filter(|f| f.dir == Direction::Sent && f.at_ms >= from_ms)
        .filter_map(|f| match &f.frame {
            Frame::PullRequest { caller: c, since, .. } if c == caller => Some((f.at_ms, *since)),
            _ => None,
        })
        .collect()
}

/// Answer whatever in-dialog OPTIONS reach the UAs.
async fn answer_options(alice: &Agent, bob: &Agent) {
    for ua in [alice, bob] {
        while let Some(mut t) = ua.try_receive_tolerating("OPTIONS", &[]).await {
            t.respond(200, "OK").await;
        }
    }
}

/// Set up a call from alice to bob through the proxy, answer it and ACK it.
async fn answered_call(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: std::net::SocketAddr,
) -> (scenario_harness::Dialog, (String, String)) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(proxy).send().await;
    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS", "ACK"]).await;
    let ordinals = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(300)).await;
    (dialog, ordinals)
}

#[tokio::test(start_paused = true)]
async fn a_silent_replication_stream_is_reopened_from_its_watermark_and_calls_never_notice() {
    let name = "silent-replication-stream";
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
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── call 1 answered and replicated ───────────────────────────────────────
    let (mut dialog1, (pri_ord, bak_ord)) = answered_call(&fh, &alice, &bob, proxy.addr()).await;
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    let ref1 = backed_up_ref(&fh, backup, &pri_ord).await;

    // ── the primary's replication toward the backup goes silent ──────────────
    fh.stall(&pri_ord, &bak_ord);
    let held_at = fh.now_ms();
    let pulls_before = pulls_by(&fh, &bak_ord, 0).len();

    // ── call 2 during the hold: set up, answered, hung up ────────────────────
    fh.advance(Duration::from_secs(2)).await;
    let (mut dialog2, _) = answered_call(&fh, &alice, &bob, proxy.addr()).await;
    fh.advance(Duration::from_secs(1)).await;
    scenario_harness::callflow::hangup(&mut dialog2, &bob).await;

    // ── past the idle bound: the backup has pulled again ─────────────────────
    let repulled = fh
        .pump_until(Duration::from_millis(200), Duration::from_secs(17), async || {
            answer_options(&alice, &bob).await;
            pulls_by(&fh, &bak_ord, held_at).iter().any(|(_, since)| *since > Watermark::new(0, 0))
        })
        .await;
    let pulls = pulls_by(&fh, &bak_ord, held_at);
    fh.mark(&bak_ord, Some(&pri_ord), "repull", &format!("{pulls:?}"));
    assert!(
        repulled,
        "the backup leaves the silent stream and pulls the primary again from its watermark \
         ({pulls_before} pulls before the hold, {pulls:?} after)"
    );
    let first = pulls.iter().find(|(_, since)| *since > Watermark::new(0, 0)).unwrap().0;
    assert!(
        first - held_at >= IDLE_MS - NOOP_MS && first - held_at <= IDLE_MS + BACKOFF_FLOOR_MS + 200,
        "re-pulled {} ms into the hold: no sooner than the idle bound after the stream's \
         last frame, no later than one backoff step past it",
        first - held_at
    );

    // ── call 1 hung up during the hold, then the hold is lifted ──────────────
    fh.advance(Duration::from_secs(2)).await;
    answer_options(&alice, &bob).await;
    scenario_harness::callflow::hangup(&mut dialog1, &bob).await;
    fh.advance(Duration::from_secs(5)).await;
    answer_options(&alice, &bob).await;
    fh.resume(&pri_ord, &bak_ord);

    // ── both calls released everywhere ───────────────────────────────────────
    let nodes: [&ReplicatedB2buaSut; 2] = [primary, backup];
    let drained = fh
        .settle_terminal(async || {
            answer_options(&alice, &bob).await;
            primary.memory_clean()
                && backup.memory_clean()
                && !primary.holds_any_trace(&ref1).await
                && !backup.holds_any_trace(&ref1).await
        })
        .await;
    assert!(drained, "both nodes release both calls once the stream flows again");
    assert_call_fully_released(&nodes[..], &ref1).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], &ref1), 1, "one CDR for call 1");
    let cdrs: usize = nodes.iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 2, "one CDR per call across the cluster");

    let written = fh
        .write_unified_report(
            &report_dir(),
            "report",
            "A silent replication stream is reopened; calls never notice",
            true,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());

    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}
