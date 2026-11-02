use std::time::Duration;

use repl_net::frame::Frame;

use super::*;
use crate::store::PartitionRole;

fn changelog() -> Changelog {
    Changelog::new(7, Clock::test_at(0))
}

#[test]
fn a_flow_below_the_head_is_not_caught_up_and_at_the_head_is() {
    let cl = changelog();
    let _guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.bump_put("A", "call-2", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "nothing reported yet");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    assert!(!cl.flow_caught_up("A", Partition::Bak), "reported below the head");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 2));
    assert!(cl.flow_caught_up("A", Partition::Bak), "reported the head");
    // The other partition has its own flow: nothing carries it.
    assert!(!cl.flow_caught_up("A", Partition::Pri));
}

#[test]
fn an_empty_sublog_is_caught_up_only_once_its_flow_reports() {
    let cl = changelog();
    assert!(!cl.flow_caught_up("A", Partition::Bak), "no peer log at all");
    let _guard = cl.serving("A", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "connected, nothing reported");
    // The first catch-up Noop reports the scan head: current from then on.
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 0));
    assert!(cl.flow_caught_up("A", Partition::Bak));
}

#[test]
fn a_reaped_peer_log_is_not_vacuously_current_on_return() {
    let cl = changelog().with_dead_peer_ttl(1_000);
    {
        let _guard = cl.serving("A", Partition::Bak);
        cl.bump_put("A", "call-1", Partition::Bak);
        cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
        assert!(cl.flow_caught_up("A", Partition::Bak));
    }
    cl.reap(10_000);
    assert!(!cl.has_peer("A"), "idle past the dead-peer TTL: the log is gone");
    let _again = cl.serving("A", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "a fresh log has no claim");
}

#[test]
fn a_second_flow_does_not_inherit_a_dead_flows_claim() {
    let cl = changelog();
    let _dead = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak));
    // The peer reconnects while its old socket is still open server-side.
    let _fresh = cl.serving("A", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "the new flow has reported nothing");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak));
}

/// A worker superseded on the ordinal vouches for nothing its successor
/// holds (ADR-0055 §6): once membership binds the ordinal to the successor's
/// identity, the old identity's reports raise nothing, on its flow still open
/// or on one it opens again.
#[tokio::test(start_paused = true)]
async fn a_superseded_identitys_report_releases_no_hold() {
    let cl = changelog();
    cl.bind_identity("A", Some("uid-1"));
    let _old = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_applied("A", Some("uid-1"), Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak), "the bound identity's report stands");
    cl.bind_identity("A", Some("uid-2"));
    let _new = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-2", Partition::Bak);
    let _old_again = cl.serving("A", Partition::Bak);
    cl.note_applied("A", Some("uid-1"), Partition::Bak, Watermark::new(7, 2));
    assert!(!cl.flow_caught_up("A", Partition::Bak), "the old identity vouches for nothing");
    let reached = cl.applied_reaches("A", Partition::Bak, Watermark::new(7, 2));
    assert!(
        tokio::time::timeout(Duration::from_millis(1), reached).await.is_err(),
        "the old identity's report releases no hold"
    );
    cl.note_applied("A", Some("uid-2"), Partition::Bak, Watermark::new(7, 2));
    assert!(cl.flow_caught_up("A", Partition::Bak), "the successor's report stands");
}

/// A membership that names no identity for the ordinal filters nothing.
#[test]
fn an_ordinal_bound_to_no_identity_counts_every_report() {
    let cl = changelog();
    cl.bind_identity("A", Some("uid-1"));
    cl.bind_identity("A", None);
    let _guard = cl.serving("A", Partition::Bak);
    cl.note_applied("A", Some("uid-2"), Partition::Bak, Watermark::new(7, 0));
    assert!(cl.flow_caught_up("A", Partition::Bak));
}

#[test]
fn a_disconnected_flow_holds_no_position() {
    let cl = changelog();
    let guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak));
    drop(guard);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "a cut flow's position is unknown");
    // A fresh flow starts from "unknown", not from the cleared claim.
    let _again = cl.serving("A", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak));
}

#[test]
fn the_default_claim_ttl_is_two_default_noop_intervals() {
    assert_eq!(DEFAULT_CLAIM_TTL, crate::repl::ServeConfig::default().claim_ttl());
}

/// A puller answers every idle `Noop` with its position, so a claim older
/// than two `Noop` intervals belongs to a flow whose peer stopped answering —
/// one that died without a FIN keeps its serve task, and its claim, alive.
#[tokio::test(start_paused = true)]
async fn a_claim_no_noop_answer_refreshed_goes_stale() {
    let cl = changelog();
    let _guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    tokio::time::advance(Duration::from_millis(900)).await;
    assert!(cl.flow_caught_up("A", Partition::Bak), "fresh inside two intervals");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    tokio::time::advance(Duration::from_millis(900)).await;
    assert!(cl.flow_caught_up("A", Partition::Bak), "the same position, re-stated");
    tokio::time::advance(Duration::from_millis(200)).await;
    assert!(!cl.flow_caught_up("A", Partition::Bak), "past two intervals: unknown");
}

/// Each served flow reads live with a fresh claim, stale once its claim is
/// older than the claim TTL or before its first, and down with no flow.
#[tokio::test(start_paused = true)]
async fn served_flows_read_live_stale_or_down() {
    let cl = changelog();
    let counts = |live, stale, down| [live, stale, down];
    let bak = cl.serving("A", Partition::Bak);
    assert_eq!(cl.flow_liveness(), counts(0, 1, 1), "connected, no claim yet; Pri not pulled");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 0));
    let _pri = cl.serving("A", Partition::Pri);
    cl.note_applied("A", None, Partition::Pri, Watermark::new(7, 0));
    assert_eq!(cl.flow_liveness(), counts(2, 0, 0));
    tokio::time::advance(DEFAULT_CLAIM_TTL + Duration::from_millis(1)).await;
    cl.note_applied("A", None, Partition::Pri, Watermark::new(7, 0));
    assert_eq!(cl.flow_liveness(), counts(1, 1, 0), "the Bak claim went stale");
    drop(bak);
    assert_eq!(cl.flow_liveness(), counts(1, 0, 1), "the Bak flow went down");
    assert_eq!(FlowLiveness::ALL.map(FlowLiveness::label), ["live", "stale", "down"]);
}

/// The latest claim stands, lower or not: a peer that shed what it held
/// claims less, and its earlier, higher claim no longer vouches for it.
#[test]
fn a_lower_claim_replaces_a_higher_one() {
    let cl = changelog();
    let _guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.bump_put("A", "call-2", Partition::Bak);
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 2));
    assert!(cl.flow_caught_up("A", Partition::Bak));
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    assert!(!cl.flow_caught_up("A", Partition::Bak), "the peer now claims less than the head");
}

#[test]
fn the_head_survives_compaction_of_the_same_ref() {
    let cl = changelog();
    let _guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    assert_eq!(cl.peer_len("A", Partition::Bak), 1, "compacted to one entry");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
    assert!(!cl.flow_caught_up("A", Partition::Bak), "counter 1 was moved to 2");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 2));
    assert!(cl.flow_caught_up("A", Partition::Bak));
}

/// A source holding no bodies: every drained entry is a `Delete`.
struct NoBodies;

#[async_trait::async_trait]
impl BodySource for NoBodies {
    async fn read_body(&self, _: PartitionRole, _: &str, _: &str) -> Option<Arc<[u8]>> {
        None
    }
    fn read_meta(&self, _: &str) -> Option<RefMeta> {
        None
    }
}

/// The `(op, answered)` of every frame `cl` drains for `A`'s Backup flow
/// from `since`.
async fn drained(cl: &Changelog, since: Watermark) -> Vec<(Op, bool)> {
    cl.drain_since("A", Partition::Bak, since, 100, &NoBodies, PartitionRole::Primary, "self")
        .await
        .into_iter()
        .map(|f| match f {
            Frame::Data { op, answered, .. } => (op, answered),
            other => panic!("expected Data, got {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_put_compacted_into_a_delete_drains_as_one_delete_stating_the_answer() {
    let cl = changelog();
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.bump_delete("A", "call-1", Partition::Bak, true, None);
    assert_eq!(drained(&cl, Watermark::new(7, 0)).await, vec![(Op::Delete, true)]);
    // A cold drain reads the same tombstone.
    assert_eq!(drained(&cl, Watermark::new(0, 0)).await, vec![(Op::Delete, true)]);
}

#[tokio::test]
async fn a_later_delete_stating_no_answer_keeps_the_stated_one() {
    let cl = changelog();
    cl.bump_delete("A", "call-1", Partition::Bak, true, None);
    cl.bump_delete("A", "call-1", Partition::Bak, false, None);
    assert_eq!(drained(&cl, Watermark::new(7, 0)).await, vec![(Op::Delete, true)]);
    // A delete that never stated one states none.
    cl.bump_delete("A", "call-2", Partition::Bak, false, None);
    assert_eq!(
        drained(&cl, Watermark::new(7, 2)).await,
        vec![(Op::Delete, false)],
        "call-2 alone is above counter 2",
    );
}

#[tokio::test]
async fn a_put_after_a_delete_starts_a_new_call_with_no_answer_stated() {
    let cl = changelog();
    cl.bump_delete("A", "call-1", Partition::Bak, true, None);
    // The same ref is reused by a new call inside the tombstone window.
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.bump_delete("A", "call-1", Partition::Bak, false, None);
    assert_eq!(
        drained(&cl, Watermark::new(7, 0)).await,
        vec![(Op::Delete, false)],
        "the earlier call's answer is not the new call's",
    );
}

/// A source whose body is deleted, with `answered`, between the drain's
/// snapshot and its body read: a local delete landing mid-drain.
struct DeletedMidDrain {
    cl: Changelog,
    answered: bool,
}

#[async_trait::async_trait]
impl BodySource for DeletedMidDrain {
    async fn read_body(&self, _: PartitionRole, _: &str, call_ref: &str) -> Option<Arc<[u8]>> {
        self.cl.bump_delete("A", call_ref, Partition::Bak, self.answered, None);
        None
    }
    fn read_meta(&self, _: &str) -> Option<RefMeta> {
        None
    }
}

#[tokio::test]
async fn a_delete_landing_mid_drain_goes_out_once_with_its_own_answer() {
    let cl = changelog();
    cl.bump_put("A", "call-1", Partition::Bak);
    let source = DeletedMidDrain { cl: cl.clone(), answered: true };
    let first = cl
        .drain_since(
            "A",
            Partition::Bak,
            Watermark::new(7, 0),
            100,
            &source,
            PartitionRole::Primary,
            "self",
        )
        .await;
    assert!(first.is_empty(), "the vanished Put leaves its delete to its own entry: {first:?}");
    assert_eq!(drained(&cl, Watermark::new(7, 1)).await, vec![(Op::Delete, true)]);
}

#[test]
fn a_position_from_an_older_incarnation_never_reaches_the_head() {
    let cl = changelog();
    let _guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_applied("A", None, Partition::Bak, Watermark::new(6, u64::MAX));
    assert!(!cl.flow_caught_up("A", Partition::Bak));
}

/// A log no flow serves goes one dead-peer TTL after a flow last served it,
/// whatever it is written meanwhile: toward an away backup it holds the
/// writes of one TTL at most.
#[tokio::test(start_paused = true)]
async fn a_log_no_flow_serves_goes_at_the_dead_peer_ttl_whatever_it_is_written() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(7, clock.clone()).with_dead_peer_ttl(2_000);
    drop(cl.serving("A", Partition::Bak));
    for i in 0..3 {
        tokio::time::advance(Duration::from_millis(900)).await;
        cl.bump_put("A", &format!("call-{i}"), Partition::Bak);
    }
    cl.reap(clock.now_ms());
    assert!(!cl.has_peer("A"), "writes do not keep a log no flow serves");
}

/// The calls the tail sends `A` from `since`, at most `limit` of them.
async fn sent(cl: &Changelog, since: Watermark, limit: usize) -> Vec<(String, Watermark)> {
    let frames = cl
        .drain_since("A", Partition::Bak, since, limit, &NoBodies, PartitionRole::Primary, "self")
        .await;
    frames
        .into_iter()
        .map(|f| match f {
            Frame::Data { call_ref, at, .. } => (call_ref, at),
            other => panic!("expected Data, got {other:?}"),
        })
        .collect()
}

/// A peer holds a call once, under the identity bound to it, it reported
/// applying a position past the first one the call was sent at, however stale
/// its copy; a new identity holds nothing an earlier one applied until it
/// reports again.
#[tokio::test]
async fn a_peer_holds_a_call_it_applied_a_write_of_under_its_identity() {
    let cl = changelog();
    cl.bind_identity("A", Some("a#g1"));
    let _flow = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    assert_eq!(sent(&cl, Watermark::new(7, 0), 10).await.len(), 1);
    assert!(!cl.applied_any("A", Partition::Bak, "call-1"), "sent, nothing applied yet");
    cl.note_applied("A", Some("a#g1"), Partition::Bak, Watermark::new(7, 1));
    cl.bump_put("A", "call-1", Partition::Bak);
    assert!(cl.applied_any("A", Partition::Bak, "call-1"), "a stale copy is a copy");
    cl.bump_put("A", "call-2", Partition::Bak);
    assert!(!cl.applied_any("A", Partition::Bak, "call-2"), "never sent");
    cl.bind_identity("A", Some("a#g2"));
    assert!(!cl.applied_any("A", Partition::Bak, "call-1"), "a restarted peer holds nothing");
    cl.note_scanned("A", Partition::Bak, cl.head());
    cl.note_applied("A", Some("a#g2"), Partition::Bak, cl.head());
    assert!(cl.applied_any("A", Partition::Bak, "call-2"), "its bootstrap carried the call");
    cl.bump_delete("A", "call-2", Partition::Bak, false, None);
    cl.bump_put("A", "call-2", Partition::Bak);
    assert!(!cl.applied_any("A", Partition::Bak, "call-2"), "a new call on the ref is not held");
}

/// A call whose entry moved before the tail reached it was never sent at its
/// first counter: a peer that applied a later position of another call does
/// not hold it.
#[tokio::test]
async fn a_peer_never_sent_a_call_does_not_hold_it() {
    let cl = changelog();
    let _flow = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-x", Partition::Bak);
    cl.bump_put("A", "call-y", Partition::Bak);
    cl.bump_put("A", "call-x", Partition::Bak);
    let one = sent(&cl, Watermark::new(7, 0), 1).await;
    assert_eq!(one, vec![("call-y".to_string(), Watermark::new(7, 2))], "one batch: call-y");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 2));
    assert!(!cl.applied_any("A", Partition::Bak, "call-x"), "call-x was never sent to the peer");
    assert_eq!(sent(&cl, Watermark::new(7, 2), 10).await.len(), 1, "call-x goes next");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 3));
    assert!(cl.applied_any("A", Partition::Bak, "call-x"), "applied once sent");
}

/// A backup away past the dead-peer TTL comes back as a new identity and
/// bootstraps cold: the scan carries every call whose metadata names it, so it
/// holds a call written before its log was dropped, which has no entry in the
/// log opened since.
#[tokio::test(start_paused = true)]
async fn a_backup_back_past_the_ttl_holds_what_its_bootstrap_carried() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(7, clock.clone()).with_dead_peer_ttl(2_000);
    cl.bind_identity("A", Some("a#1"));
    {
        let _flow = cl.serving("A", Partition::Bak);
        cl.bump_put("A", "call-x", Partition::Bak);
        assert_eq!(sent(&cl, Watermark::new(7, 0), 10).await.len(), 1);
        cl.note_applied("A", Some("a#1"), Partition::Bak, Watermark::new(7, 1));
        assert!(cl.applied_any("A", Partition::Bak, "call-x"), "the peer applied call-x");
    }
    tokio::time::advance(Duration::from_secs(3)).await;
    cl.reap(clock.now_ms());
    assert!(!cl.has_peer("A"), "the log went at the TTL");
    cl.bind_identity("A", Some("a#2"));
    let _flow = cl.serving("A", Partition::Bak);
    assert!(!cl.applied_any("A", Partition::Bak, "call-x"), "nothing reported yet");
    cl.note_scanned("A", Partition::Bak, cl.head());
    cl.note_applied("A", Some("a#2"), Partition::Bak, cl.head());
    assert!(cl.applied_any("A", Partition::Bak, "call-x"), "the cold bootstrap carried call-x");
}

/// A restarted peer's report that reaches the log before membership binds its
/// new identity counts once the identity is bound.
#[test]
fn a_report_made_before_its_identity_is_bound_counts_once_bound() {
    let cl = changelog();
    cl.bind_identity("A", Some("a#g1"));
    let _flow = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_scanned("A", Partition::Bak, cl.head());
    cl.note_applied("A", Some("a#g2"), Partition::Bak, cl.head());
    assert!(!cl.applied_any("A", Partition::Bak, "call-1"), "a#g2 is not bound yet");
    cl.bind_identity("A", Some("a#g2"));
    assert!(cl.applied_any("A", Partition::Bak, "call-1"), "its report counts once bound");
}

/// A flow sends call-x and breaks before the peer applies it; call-x is
/// written again and moves; the next flow resumes below it and the peer
/// applies a later position of another call: it never applied call-x.
#[tokio::test]
async fn a_send_lost_with_its_flow_does_not_hold_a_moved_call() {
    let cl = changelog();
    {
        let _first = cl.serving("A", Partition::Bak);
        cl.bump_put("A", "call-z", Partition::Bak);
        assert_eq!(sent(&cl, Watermark::new(7, 0), 10).await.len(), 1);
        cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 1));
        cl.bump_put("A", "call-x", Partition::Bak);
        cl.bump_put("A", "call-y", Partition::Bak);
        assert_eq!(sent(&cl, Watermark::new(7, 1), 10).await.len(), 2, "lost with the socket");
    }
    cl.bump_put("A", "call-x", Partition::Bak);
    let _second = cl.serving("A", Partition::Bak);
    let next = sent(&cl, Watermark::new(7, 1), 1).await;
    assert_eq!(next, vec![("call-y".to_string(), Watermark::new(7, 3))], "warm from 1: call-y");
    cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 3));
    assert!(!cl.applied_any("A", Partition::Bak, "call-x"), "the peer never applied call-x");
}

/// The marks used by the bootstrap tests: over them at three pending entries.
fn small_marks() -> BacklogMarks {
    BacklogMarks {
        high_entries: 3,
        low_entries: 1,
        high_age: Duration::from_secs(5),
        low_age: Duration::from_secs(1),
    }
}

/// A backup whose flow re-opens every 5 s and never claims a position is one
/// bootstrap dated from its first open: past the hard deadline it is held to
/// the marks.
#[tokio::test(start_paused = true)]
async fn a_bootstrap_reopened_without_a_claim_is_held_to_the_marks() {
    let log = changelog();
    log.set_backlog_marks(small_marks());
    for i in 0..3 {
        log.bump_put("B", &format!("c{i}"), Partition::Bak);
    }
    for _ in 0..3 {
        let flow = log.serving("B", Partition::Bak);
        tokio::time::advance(Duration::from_secs(5)).await;
        drop(flow);
    }
    let _flow = log.serving("B", Partition::Bak);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!log.admits_backup("B"), "16 s of bootstrap with no claim, over both marks");
}

/// A peer whose flows open and close without ever claiming does not keep its
/// log: it goes one dead-peer TTL after the last claim, at a reap with no flow
/// open.
#[tokio::test(start_paused = true)]
async fn a_log_toward_a_peer_that_never_claims_goes_one_ttl_after_its_last_claim() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(7, clock.clone()).with_dead_peer_ttl(2_000);
    {
        let _flow = cl.serving("A", Partition::Bak);
        cl.note_applied("A", None, Partition::Bak, Watermark::new(7, 0));
    }
    for i in 0..3 {
        let flow = cl.serving("A", Partition::Bak);
        cl.bump_put("A", &format!("call-{i}"), Partition::Bak);
        tokio::time::advance(Duration::from_millis(700)).await;
        drop(flow);
        tokio::time::advance(Duration::from_millis(100)).await;
    }
    cl.reap(clock.now_ms());
    assert!(!cl.has_peer("A"), "2.4 s past the last claim, no flow open");
}

/// A cold pull from the bound identity states it holds nothing (a restarted
/// process under the same identity): the calls it applied, on either
/// partition, read unheld until its bootstrap claims again. A cold pull from
/// an identity not bound changes nothing.
#[tokio::test]
async fn a_cold_pull_from_the_bound_identity_holds_nothing_until_it_claims() {
    let cl = changelog();
    cl.bind_identity("A", Some("a#1"));
    let _flow = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    assert_eq!(sent(&cl, Watermark::new(7, 0), 10).await.len(), 1);
    cl.note_applied("A", Some("a#1"), Partition::Bak, Watermark::new(7, 1));
    assert!(cl.applied_any("A", Partition::Bak, "call-1"));
    let _reclaim = cl.serving("A", Partition::Pri);
    cl.bump_put("A", "copy-1", Partition::Pri);
    cl.note_scanned("A", Partition::Pri, cl.head());
    cl.note_applied("A", Some("a#1"), Partition::Pri, cl.head());
    assert!(cl.applied_any("A", Partition::Pri, "copy-1"));
    cl.note_cold_pull("A", Some("a#9"));
    assert!(cl.applied_any("A", Partition::Bak, "call-1"), "an unbound identity changes nothing");
    cl.note_cold_pull("A", Some("a#1"));
    assert!(!cl.applied_any("A", Partition::Bak, "call-1"), "the restarted process holds nothing");
    assert!(!cl.applied_any("A", Partition::Pri, "copy-1"), "on either partition");
    cl.note_scanned("A", Partition::Bak, cl.head());
    cl.note_applied("A", Some("a#1"), Partition::Bak, cl.head());
    assert!(cl.applied_any("A", Partition::Bak, "call-1"), "its bootstrap carried the call");
}
