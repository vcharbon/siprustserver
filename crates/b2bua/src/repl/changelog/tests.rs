use super::*;

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
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 1));
    assert!(!cl.flow_caught_up("A", Partition::Bak), "reported below the head");
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 2));
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
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 0));
    assert!(cl.flow_caught_up("A", Partition::Bak));
}

#[test]
fn a_reaped_peer_log_is_not_vacuously_current_on_return() {
    let cl = changelog().with_ttls(1_000, 1_000);
    {
        let _guard = cl.serving("A", Partition::Bak);
        cl.bump_put("A", "call-1", Partition::Bak);
        cl.note_applied("A", Partition::Bak, Watermark::new(7, 1));
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
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak));
    // The peer reconnects while its old socket is still open server-side.
    let _fresh = cl.serving("A", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "the new flow has reported nothing");
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak));
}

#[test]
fn a_disconnected_flow_holds_no_position() {
    let cl = changelog();
    let guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 1));
    assert!(cl.flow_caught_up("A", Partition::Bak));
    drop(guard);
    assert!(!cl.flow_caught_up("A", Partition::Bak), "a cut flow's position is unknown");
    // A fresh flow starts from "unknown", not from the cleared claim.
    let _again = cl.serving("A", Partition::Bak);
    assert!(!cl.flow_caught_up("A", Partition::Bak));
}

#[test]
fn the_head_survives_compaction_of_the_same_ref() {
    let cl = changelog();
    let _guard = cl.serving("A", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    cl.bump_put("A", "call-1", Partition::Bak);
    assert_eq!(cl.peer_len("A", Partition::Bak), 1, "compacted to one entry");
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 1));
    assert!(!cl.flow_caught_up("A", Partition::Bak), "counter 1 was moved to 2");
    cl.note_applied("A", Partition::Bak, Watermark::new(7, 2));
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
    cl.note_applied("A", Partition::Bak, Watermark::new(6, u64::MAX));
    assert!(!cl.flow_caught_up("A", Partition::Bak));
}
