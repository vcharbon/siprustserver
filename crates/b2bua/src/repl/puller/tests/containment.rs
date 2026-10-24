//! Two copies of one call meeting in the store (ADR-0055 §7): a write that
//! read the stored version is applied, the copy that contains the other
//! stands at the higher counters, the primary's when each contains the
//! other, and a write that diverged leaves the stored copy.

use std::sync::Arc;

use repl_net::frame::{Op, Partition};

use super::{backup_puller, body_at};
use super::{ApplyMode, ApplyOutcome, Puller};
use crate::repl::ReplicatingCallStore;
use crate::store::PartitionRole;

/// A `Put` of `body` on the flow `partition` in `mode`, at the version and
/// under the index keys the body states.
async fn put_body(
    p: &Puller,
    partition: Partition,
    mode: ApplyMode,
    body: Arc<[u8]>,
) -> ApplyOutcome {
    use call::CallBodyCodec;
    let call = call::MsgpackCodec::new().decode(&body).expect("decodes");
    let indexes = call::call_index_keys(&call);
    let (gen, bgen) = call.topology.expect("replicated").vector();
    p.apply_to_store(
        Op::Put,
        partition,
        "w0|split|t",
        gen,
        bgen,
        60_000,
        0,
        &indexes,
        Some(body),
        false,
        None,
        mode,
    )
    .await
}

/// A forward `Put` of `body`.
async fn forward(p: &Puller, body: Arc<[u8]>) -> ApplyOutcome {
    put_body(p, Partition::Bak, ApplyMode::Forward, body).await
}

/// The stored copy of the split call in `role`, decoded.
async fn stored(store: &ReplicatingCallStore, role: PartitionRole) -> call::Call {
    use call::CallBodyCodec;
    let raw = store.peek_body_raw(role, "w0", "w0|split|t").await.expect("held");
    call::MsgpackCodec::new().decode(&raw).expect("decodes")
}

/// A forward write at or past the Element's version on both counters read
/// every write the Element holds and is applied, whatever its body; one the
/// Element's version dominates is a version it read.
#[tokio::test]
async fn a_write_that_read_the_elements_version_is_applied() {
    use call::CallModelState::{Active, Terminated};
    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    let answered = body_at(Some(200), Active, (6, 3), (6, 3));
    assert_eq!(forward(&p, answered).await, ApplyOutcome::Applied);
    let ended = body_at(Some(480), Terminated, (7, 3), (7, 3));
    assert_eq!(forward(&p, ended).await, ApplyOutcome::Applied, "it read the answer");
    assert_eq!(store.current_cv(PartitionRole::Backup, "w0", "w0|split|t"), Some((7, 3)));
    let again = body_at(Some(480), Terminated, (6, 3), (6, 3));
    assert_eq!(forward(&p, again).await, ApplyOutcome::Dominated);
}

/// Two copies that each made a compared write the other never read: the
/// Element keeps its own and the write rides up to the live copy.
#[tokio::test]
async fn a_write_that_diverged_from_the_element_is_refused_and_leaves_it() {
    use call::CallModelState::{Active, Terminated};
    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    let copy = body_at(Some(200), Active, (5, 3), (5, 3));
    assert_eq!(forward(&p, copy).await, ApplyOutcome::Applied);
    let branch = body_at(Some(480), Terminated, (6, 2), (6, 2));
    assert_eq!(forward(&p, branch).await, ApplyOutcome::Refused);
    assert_eq!(store.current_cv(PartitionRole::Backup, "w0", "w0|split|t"), Some((5, 3)));
    assert!(call::helpers::caller_answered(&stored(&store, PartitionRole::Backup).await));
}

/// A write that contains the Element — the Element's own writes since the
/// copies branched changed exempt fields alone — is applied at the higher of
/// each counter; an Element that contains the write keeps its own body and
/// takes the higher counters.
#[tokio::test]
async fn the_copy_that_contains_the_other_stands_at_the_higher_counters() {
    use call::CallModelState::Active;
    let r = "w0|split|t";
    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    assert_eq!(forward(&p, body_at(None, Active, (5, 3), (5, 2))).await, ApplyOutcome::Applied);
    let answered = body_at(Some(200), Active, (6, 2), (6, 2));
    assert_eq!(forward(&p, answered).await, ApplyOutcome::Applied, "it contains the Element");
    assert_eq!(store.current_cv(PartitionRole::Backup, "w0", r), Some((6, 3)));
    let held = stored(&store, PartitionRole::Backup).await;
    assert!(call::helpers::caller_answered(&held), "the primary's body");
    assert_eq!(held.topology.expect("replicated").stamp(), (6, 2), "and its stamp");

    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    let copy = body_at(Some(200), Active, (5, 3), (5, 3));
    assert_eq!(forward(&p, copy).await, ApplyOutcome::Applied);
    let keepalive = body_at(None, Active, (6, 2), (5, 2));
    assert_eq!(forward(&p, keepalive).await, ApplyOutcome::Refused, "the Element contains it");
    assert_eq!(store.current_cv(PartitionRole::Backup, "w0", r), Some((6, 3)));
    let held = stored(&store, PartitionRole::Backup).await;
    assert!(call::helpers::caller_answered(&held), "the Element's own body");
}

/// Copies that contain each other: the primary's stands — the incoming one on
/// the Forward flow, the stored one on the Reverse flow.
#[tokio::test]
async fn copies_that_contain_each_other_keep_the_primarys_in_the_store() {
    use call::CallModelState::Active;
    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    assert_eq!(forward(&p, body_at(None, Active, (4, 3), (4, 1))).await, ApplyOutcome::Applied);
    let primary = body_at(Some(200), Active, (5, 1), (4, 1));
    assert_eq!(forward(&p, primary).await, ApplyOutcome::Applied);
    assert!(call::helpers::caller_answered(&stored(&store, PartitionRole::Backup).await));

    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    let own = body_at(Some(200), Active, (5, 1), (4, 1));
    assert_eq!(
        put_body(&p, Partition::Pri, ApplyMode::Bootstrap, own).await,
        ApplyOutcome::Applied
    );
    let copy = body_at(None, Active, (4, 3), (4, 1));
    let outcome = put_body(&p, Partition::Pri, ApplyMode::Reverse, copy).await;
    assert_eq!(outcome, ApplyOutcome::Refused, "the record's own stands");
    let held = stored(&store, PartitionRole::Primary).await;
    assert!(call::helpers::caller_answered(&held));
    assert_eq!(held.topology.expect("replicated").vector(), (5, 3));
}

/// `body` with a callback context, an index key only it carries.
fn with_callback(body: Arc<[u8]>, ctx: &str) -> Arc<[u8]> {
    use call::CallBodyCodec;
    let codec = call::MsgpackCodec::new();
    let mut call = codec.decode(&body).expect("decodes");
    call.callback_context = Some(ctx.into());
    Arc::from(codec.encode(&call).unwrap())
}

/// An Element kept over a write it contains keeps its own index keys, not the
/// write's: a key only the Element's call carries still resolves to it.
#[tokio::test]
async fn a_kept_element_keeps_its_own_index_keys() {
    use crate::store::CallStore;
    use call::CallModelState::Active;
    let (p, store, _, _) = backup_puller(crate::capacity::BackupCeilings::default());
    let copy = with_callback(body_at(Some(200), Active, (5, 3), (5, 3)), "kept-ctx");
    assert_eq!(forward(&p, copy).await, ApplyOutcome::Applied);
    let keepalive = body_at(None, Active, (6, 2), (5, 2));
    assert_eq!(forward(&p, keepalive).await, ApplyOutcome::Refused, "the Element contains it");
    let key = call::index_key::callback_key("kept-ctx");
    let resolved = store.get_index(&key).await.expect("the index reads");
    assert_eq!(resolved.as_deref(), Some("w0|split|t"), "the Element's own key stays");
}
