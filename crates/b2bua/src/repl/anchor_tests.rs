//! The clock frame a drained body is stamped in (ADR-0055 §9): the frame of
//! the instants the body holds, so the receiver's offset composes every hop.

use repl_net::frame::{Frame, Partition, Watermark};
use sip_clock::Clock;

use super::ReplicatingCallStore;
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const SELF: &str = "w0";

async fn put(store: &ReplicatingCallStore, body: &[u8], call_gen: i64, opts: &PutOpts) {
    store.put_call(PRI, SELF, "c1", body.to_vec(), &[], 0, call_gen, 0, opts).await.unwrap();
}

/// The `origin_now_ms` of the one frame `store` drains for `A` above `since`.
async fn drained_origin(store: &ReplicatingCallStore, since: u64) -> i64 {
    let frames = store
        .changelog()
        .drain_since("A", Partition::Bak, Watermark::new(7, since), usize::MAX, store, PRI, SELF)
        .await;
    match frames.as_slice() {
        [Frame::Data { origin_now_ms, .. }] => *origin_now_ms,
        other => panic!("expected one Data frame, got {other:?}"),
    }
}

/// A frame carrying a body states its send time in the frame of the body's
/// instants (ADR-0055 §9): a body applied from a peer goes out less the offset
/// it is stored with, so the next receiver's offset composes both hops; a body
/// written here goes out at this node's clock.
#[tokio::test(start_paused = true)]
async fn a_drained_body_is_stamped_in_the_frame_of_its_instants() {
    let store = ReplicatingCallStore::new(7, Clock::test_at(100_000));
    put(&store, b"v1", 1, &PutOpts { origin_now_ms: Some(70_000), ..PutOpts::default() }).await;
    store.changelog().bump_put("A", "c1", Partition::Bak);
    assert_eq!(
        drained_origin(&store, 0).await,
        70_000,
        "the applied body goes out in its writer's frame"
    );

    let forward = PutOpts {
        peer: Some("A".into()),
        direction: Some(PropagateDirection::Forward),
        ..PutOpts::default()
    };
    put(&store, b"v2", 2, &forward).await;
    assert_eq!(
        drained_origin(&store, 1).await,
        100_000,
        "a body written here goes out at this clock"
    );
}
