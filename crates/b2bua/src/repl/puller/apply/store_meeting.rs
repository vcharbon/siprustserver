//! Two copies of one call meeting in the store (ADR-0055 §7), when neither
//! version descends from the other: read in one clock frame (§9), the copy
//! that contains the other stands with the other's exempt fields merged in.

use std::sync::Arc;

use call::helpers::instants::reanchor_offset;
use call::helpers::shift_instants;

use super::super::Puller;
use crate::store::containment::{meet, Met};
use crate::store::envelope::{self, Enveloped};
use crate::store::{CallStore, PartitionRole, PutOpts};

/// What the store takes of an incoming copy that contains the stored one:
/// the merged body and its version, `None` when it could not be written out.
pub(super) struct Taken {
    pub(super) body: Option<Arc<[u8]>>,
    pub(super) version: Option<(i64, i64)>,
}

impl Puller {
    /// The copy stored for `call_ref` meets `incoming`, whose sender's clock
    /// read `origin_now_ms`, the stored one moved into the incoming one's
    /// frame first; when each contains the other the primary's stands, the
    /// incoming one when `primary_incoming`. The incoming copy that contains
    /// the stored one is [`Taken`] in its own frame; a stored copy that
    /// contains it is kept, the incoming copy's exempt fields merged in and
    /// its frame its own (`kept`); copies that diverged, or a copy either side
    /// cannot read, leave the store as it is. `None` unless taken.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn meet_stored(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        incoming: Option<&Enveloped>,
        primary_incoming: bool,
        origin_now_ms: i64,
        kept: KeptCopy,
    ) -> Option<Taken> {
        let mut held = self.stored_copy(role, primary, call_ref).await?;
        let gap = self.frame_gap(call_ref, origin_now_ms);
        shift_instants(&mut held.call, gap);
        match meet(held, incoming?, primary_incoming) {
            Met::Incoming(merged) => {
                let version = merged.topology.as_ref().map(|t| t.vector());
                let encoded = envelope::encode(&self.codec, merged.topology.as_ref(), &merged.call);
                Some(Taken { body: encoded.ok().map(Arc::from), version })
            }
            Met::Held(mut merged) => {
                shift_instants(&mut merged.call, -gap);
                self.keep_merged(role, primary, call_ref, &merged, kept).await;
                None
            }
            Met::Diverged => None,
        }
    }

    /// The stored copy of `call_ref`, decoded; `None` for an absent one or one
    /// the model cannot read. Read with the **non-evicting**
    /// [`peek_body_raw`](crate::repl::ReplicatingCallStore::peek_body_raw), so
    /// the meeting never destroys it.
    async fn stored_copy(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<Enveloped> {
        let raw = self.store.peek_body_raw(role, primary, call_ref).await?;
        self.decode_counted(&raw)
    }

    /// How far the instants of the copy stored for `call_ref` move into the
    /// frame of an incoming copy whose sender's clock read `origin_now_ms`:
    /// each frame's offset from this node's clock as a re-anchor applies it
    /// (ADR-0055 §9), the stored copy's less the incoming one's. A body
    /// written here has no offset.
    fn frame_gap(&self, call_ref: &str, origin_now_ms: i64) -> i64 {
        let stored = self.store.skew_offset_ms(call_ref).unwrap_or(0);
        reanchor_offset(stored) - reanchor_offset(self.store.skew_of(origin_now_ms))
    }

    /// Store `merged`, the stored copy with an incoming copy's exempt fields
    /// merged in, at its own vector and under its own index keys: a local
    /// apply, never re-propagated, its instants in the frame the stored copy's
    /// were.
    async fn keep_merged(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        merged: &Enveloped,
        kept: KeptCopy,
    ) {
        let encoded = envelope::encode(&self.codec, merged.topology.as_ref(), &merged.call);
        let (Ok(body), Some(t)) = (encoded, merged.topology.as_ref()) else {
            return;
        };
        let (p, b) = t.vector();
        let opts = PutOpts {
            origin_now_ms: self.store.held_origin_now_ms(call_ref),
            incarnation: kept.incarnation,
            ..PutOpts::default()
        };
        let indexes = call::call_index_keys(&merged.call);
        let _ = self
            .store
            .put_call(role, primary, call_ref, body, &indexes, kept.body_ttl_ms, p, b, &opts)
            .await;
    }
}

/// What a stored copy kept over an incoming one is written with.
pub(super) struct KeptCopy {
    pub(super) body_ttl_ms: i64,
    pub(super) incarnation: Option<String>,
}
