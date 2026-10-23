//! The drain: a sub-log's due entries as [`Frame::Data`], bodies read live at
//! send time with the changelog lock dropped (ADR-0011 X8).

use repl_net::frame::{Frame, Op, Partition, Watermark};

use super::{BodySource, Changelog, Due};
use crate::store::{locked, PartitionRole, REPLICA_CHANGELOG};

impl Changelog {
    /// Drain a `(peer, partition)` sub-log's entries with `counter > since.counter`
    /// (or **all** when `since.gen < self.gen` — the cold/bootstrap case),
    /// ascending, **capped at `limit`**, as [`Frame::Data`]. Bodies are read
    /// **live** from `source` with the changelog lock **dropped**.
    ///
    /// `role`/`primary` locate the body in the store's keyspace (fixed by the
    /// flow: `Pri` ⇒ `(Backup, caller)`, `Bak` ⇒ `(Primary, self)`). A `Delete`
    /// ref or a vanished body yields `Frame::Data{ op: Delete, body: None, .. }`,
    /// carrying the answer the ref's tombstone states.
    #[allow(clippy::too_many_arguments)]
    pub async fn drain_since(
        &self,
        peer: &str,
        partition: Partition,
        since: Watermark,
        limit: usize,
        source: &dyn BodySource,
        role: PartitionRole,
        primary: &str,
    ) -> Vec<Frame> {
        // Brief lock: snapshot the due entries, then DROP the guard before any
        // body read/await.
        let due: Vec<Due> = {
            let inner = locked(&self.inner, REPLICA_CHANGELOG);
            let Some(log) = inner.peers.get(peer) else {
                return Vec::new();
            };
            let cold = since.gen < self.gen;
            log.sub(partition).due(since.counter, cold, limit)
        };

        let mut frames = Vec::with_capacity(due.len());
        for Due { counter, call_ref, op, answered, incarnation } in due {
            let at = Watermark::new(self.gen, counter);
            if op == Op::Delete {
                frames.push(delete_frame(at, partition, call_ref, answered, incarnation));
                continue;
            }
            // Body read at send time — lock already dropped. The frame is
            // stamped at send time, in the frame of the body's instants, so the
            // receiver re-anchors them against the clock that wrote them.
            match source.read_entry(role, primary, &call_ref).await {
                Some((body, meta)) => frames.push(Frame::Data {
                    at,
                    op: Op::Put,
                    partition,
                    call_ref,
                    call_gen: meta.call_gen,
                    call_bgen: meta.call_bgen,
                    body_ttl_ms: meta.body_ttl_ms,
                    origin_now_ms: meta.origin_now_ms(self.clock.now_ms()),
                    indexes: meta.indexes,
                    body: Some(body),
                    answered: false,
                    incarnation: meta.incarnation,
                }),
                // Gone between snapshot and read: a delete recorded since then
                // has an entry of its own and drains with its own answer; any
                // other loss (an expired body) goes out as a delete stating
                // what the entry states now.
                None => match self.current(peer, partition, &call_ref) {
                    Some((Op::Delete, _)) => {}
                    current => {
                        let answered = current.is_some_and(|(_, answered)| answered);
                        frames.push(delete_frame(at, partition, call_ref, answered, None));
                    }
                },
            }
        }
        frames
    }

    /// The `(op, answered)` `call_ref`'s entry holds now, `None` when it has none.
    fn current(&self, peer: &str, partition: Partition, call_ref: &str) -> Option<(Op, bool)> {
        let inner = locked(&self.inner, REPLICA_CHANGELOG);
        let st = inner.peers.get(peer)?.sub(partition).by_ref.get(call_ref)?;
        Some((st.op, st.answered))
    }
}

/// Build a `Delete` `Data` frame (nil body, zero meta) at `at`/`partition`,
/// stating `answered` (ADR-0031 D3) and naming the incarnation it removed.
///
/// `origin_now_ms` is `0`: a delete carries no re-anchorable timers, so the
/// receiver-side skew offset is never read for it.
fn delete_frame(
    at: Watermark,
    partition: Partition,
    call_ref: String,
    answered: bool,
    incarnation: Option<String>,
) -> Frame {
    Frame::Data {
        at,
        op: Op::Delete,
        partition,
        call_ref,
        call_gen: 0,
        call_bgen: 0,
        body_ttl_ms: 0,
        origin_now_ms: 0,
        indexes: Vec::new(),
        body: None,
        answered,
        incarnation,
    }
}
