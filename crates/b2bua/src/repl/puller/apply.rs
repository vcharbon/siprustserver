//! How a [`Puller`] reconciles an inbound `Data` frame into the local store
//! under the ADR-0014 `(p,b)` rule, and what it hands up to the router.

use std::sync::Arc;

use call::CallModelState;
use repl_net::frame::{Op, Partition};

use super::super::incarnation::is_another_call;
use super::Puller;
use crate::router::{PrimaryWrite, ReplCommand};
use crate::store::containment::{meet, Met};
use crate::store::envelope::{self, Enveloped};
use crate::store::{partition_of, CallStore, PartitionRole, PutOpts};

/// How an inbound `Data` frame is reconciled into the local store
/// ([`Puller::apply_to_store`]). Selected per frame from `(flow, bootstrapped)`
/// — one mode per direction, because the direction is what says whose progress
/// the frame may overwrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ApplyMode {
    /// Reclaim flow before its first catch-up `Noop` — a node recovering its
    /// own partition takes the replica's `(p,b)` as-is (recovery, not a merge),
    /// skipping only a strictly dominated copy (ADR-0014 §3).
    Bootstrap,
    /// Backup flow — a primary→backup update, taken when it contains the
    /// Element (ADR-0055 §7).
    Forward,
    /// Reclaim-flow steady tail — an acting backup's update, taken when it
    /// contains the stored copy (ADR-0055 §7).
    Reverse,
}

/// What [`Puller::apply_to_store`] did with a `Data` frame. `Dominated` and
/// `Refused` both leave the stored body in place but mean opposite things: a
/// dominated frame is a version the stored copy already read, a refused one is
/// a copy the stored one is not contained in — the stored copy contains it, or
/// the two diverged — and only that case is counted and handed to the meeting
/// at the live copy (ADR-0055 §7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ApplyOutcome {
    Applied,
    Dominated,
    Refused,
    /// A backup replica of a call not held yet, refused at a backup ceiling,
    /// or of a call whose replica this backup refused before (ADR-0037 item 4):
    /// the call has no backup here for life.
    Shed,
    /// A reverse flush of a call incarnation other than the Element's: a
    /// takeover copy's view of a call the authority has since replaced on the
    /// ref. The Element keeps its call and nothing is handed to the meeting.
    Superseded,
}

impl Puller {
    /// Apply a `Data` frame's mutation to the local store, **without**
    /// touching the watermark.
    ///
    /// `partition = Bak` → store as `bak:{primary}` (a **Forward** primary→backup
    /// update); `partition = Pri` → store as `pri:{primary}` (a **Reverse**
    /// backup→primary update, or a bootstrap import).
    ///
    /// A `Put` (`(call_gen, call_bgen)` = incoming `(p,b)`; `(sp, sb)` = stored):
    ///
    /// - [`Bootstrap`](ApplyMode::Bootstrap): applied unless `(sp, sb)`
    ///   dominates — recovery, not a meeting (ADR-0014 §3);
    /// - [`Forward`](ApplyMode::Forward) and [`Reverse`](ApplyMode::Reverse):
    ///   dominated by `(sp, sb)` ⇒ a version the stored copy read; at or past
    ///   `(sp, sb)` on both components ⇒ a copy that read every write the
    ///   stored one holds, applied; otherwise the two copies meet (ADR-0055 §7,
    ///   [`meet`]): the incoming copy contains the stored one ⇒ applied, the
    ///   stored copy's exempt fields merged in; the stored copy contains it ⇒
    ///   refused, the incoming copy's exempt fields merged into the stored one;
    ///   neither ⇒ refused, the store untouched. When each contains the other
    ///   the primary's stands: the incoming copy on the Forward flow, the stored
    ///   one on the Reverse flow.
    ///
    /// A `Delete` applies, except on the Forward flow over an Element the
    /// authority never saw answered (ADR-0031, "the `Delete` guard keys on the
    /// answer").
    ///
    /// With no local copy every mode applies; the store's own resurrection
    /// tombstone, not this gate, is what stops a re-create of a just-deleted call.
    /// An EXPIRED Element is already gone by the time the gate reads it — it has
    /// no vector to compare, so a `Delete` lands and a `Put` is taken as a create.
    ///
    /// The vector orders versions of one call incarnation. A `Put` of another
    /// incarnation than the Element's is not a version of it: Bootstrap and
    /// Forward take it as a create (the authority serves a new call on the ref,
    /// so the Element's call is over there), and Reverse returns
    /// [`Superseded`](ApplyOutcome::Superseded).
    ///
    /// Returns the [`ApplyOutcome`]: a dominated frame and a refusal both leave
    /// the stored body in place, and only the refusal hands the frame's body up
    /// to the meeting at the live copy.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_to_store(
        &self,
        op: Op,
        partition: Partition,
        call_ref: &str,
        call_gen: i64,
        call_bgen: i64,
        body_ttl_ms: i64,
        origin_now_ms: i64,
        indexes: &[String],
        body: Option<Arc<[u8]>>,
        answered: bool,
        incarnation: Option<String>,
        mode: ApplyMode,
    ) -> ApplyOutcome {
        let primary = partition_of(&self.self_ordinal, call_ref).1;
        let role = match partition {
            Partition::Bak => PartitionRole::Backup,
            Partition::Pri => PartitionRole::Primary,
        };

        match op {
            Op::Put => {
                let stored = self.store.current_cv(role, &primary, call_ref);
                let another_call = stored.is_some()
                    && is_another_call(
                        self.store.incarnation(call_ref).as_deref(),
                        incarnation.as_deref(),
                    );
                // The Forward direction reads the frame's body for the
                // authority's view of the answer, and both directions for a
                // meeting; it is decoded once, here.
                let incoming = matches!(mode, ApplyMode::Forward | ApplyMode::Reverse)
                    .then(|| body.as_deref().and_then(|b| self.decode_counted(b)))
                    .flatten();
                // Stored `(sp,sb)` DOMINATES incoming `(p,b)` ⇒ skip (idempotent /
                // reordered re-delivery). The dominance gate is the ONLY
                // idempotency (no watermark apply-gate): it is what makes every
                // bootstrap frame — all sharing `at = W` — safe to apply.
                let dominated = |sp: i64, sb: i64| sp >= call_gen && sb >= call_bgen;
                let descends = |sp: i64, sb: i64| sp <= call_gen && sb <= call_bgen;
                let mut body = body;
                let mut version = (call_gen, call_bgen);
                let outcome = match mode {
                    // Bootstrap recovery: the authority's body, monotone by `(p,b)`.
                    ApplyMode::Bootstrap => match stored {
                        Some((sp, sb)) if !another_call && dominated(sp, sb) => {
                            ApplyOutcome::Dominated
                        }
                        _ => ApplyOutcome::Applied,
                    },
                    ApplyMode::Forward if another_call => ApplyOutcome::Applied,
                    ApplyMode::Reverse if another_call => ApplyOutcome::Superseded,
                    ApplyMode::Forward | ApplyMode::Reverse => match stored {
                        None => ApplyOutcome::Applied,
                        Some((sp, sb)) if dominated(sp, sb) => ApplyOutcome::Dominated,
                        Some((sp, sb)) if descends(sp, sb) => ApplyOutcome::Applied,
                        Some(_) => {
                            let primary_incoming = mode == ApplyMode::Forward;
                            let held = self.stored_copy(role, &primary, call_ref).await;
                            match (held, incoming.as_ref()) {
                                (Some(held), Some(theirs)) => {
                                    match meet(held, theirs, primary_incoming) {
                                        Met::Incoming(merged) => {
                                            version = merged
                                                .topology
                                                .as_ref()
                                                .map_or(version, |t| t.vector());
                                            body = envelope::encode(
                                                &self.codec,
                                                merged.topology.as_ref(),
                                                &merged.call,
                                            )
                                            .ok()
                                            .map(Arc::from)
                                            .or(body);
                                            ApplyOutcome::Applied
                                        }
                                        Met::Held(merged) => {
                                            let kept = KeptCopy {
                                                body_ttl_ms,
                                                incarnation: incarnation.clone(),
                                            };
                                            self.keep_merged(
                                                role, &primary, call_ref, &merged, kept,
                                            )
                                            .await;
                                            ApplyOutcome::Refused
                                        }
                                        Met::Diverged => ApplyOutcome::Refused,
                                    }
                                }
                                _ => ApplyOutcome::Refused,
                            }
                        }
                    },
                };
                let outcome = if outcome == ApplyOutcome::Applied
                    && stored.is_none()
                    && role == PartitionRole::Backup
                    && !self.store.buries(call_ref, incarnation.as_deref())
                    && (self.store.refused_replica(call_ref, incarnation.as_deref())
                        || self
                            .capacity
                            .as_ref()
                            .and_then(|g| g.refuses_backup(self.store.backup_held()))
                            .is_some())
                {
                    ApplyOutcome::Shed
                } else {
                    outcome
                };
                if outcome == ApplyOutcome::Refused && mode == ApplyMode::Forward {
                    self.metrics.record_repl_forward_flush_refused("put");
                }
                if outcome == ApplyOutcome::Applied {
                    let body = body.as_deref().map(<[u8]>::to_vec).unwrap_or_default();
                    let _ = self
                        .store
                        .put_call(
                            role,
                            &primary,
                            call_ref,
                            body,
                            indexes,
                            body_ttl_ms,
                            version.0,
                            version.1,
                            // Apply locally only — do NOT re-propagate (peer:None).
                            // Carry the origin wall clock so the store keeps the
                            // body's skew offset, which re-anchors its instants on
                            // failover/reclaim (ADR-0055 §9).
                            &PutOpts {
                                origin_now_ms: Some(origin_now_ms),
                                incarnation,
                                ..PutOpts::default()
                            },
                        )
                        .await;
                    // Inbound replica admitted — record the op per stream+endpoint:
                    // a brand-new ref (`stored.is_none()`) is a create, an existing
                    // one an update. The `recovery`/`create` step is the rebooted
                    // primary's bulk reclaim made visible; `backup`/`create` is
                    // steady forward replication landing.
                    let op = if stored.is_none() { "create" } else { "update" };
                    self.metrics.record_repl_applied(self.flow_label(), self.peer_ordinal(), op);
                }
                // Every forward flush shows the authority's own view of the call,
                // whether or not the Element takes its body: latch the answer for
                // the `Delete` guard. Refusing a body is a decision about which
                // version the Element keeps, never about what its sender saw.
                if incoming.as_ref().is_some_and(|copy| call::helpers::caller_answered(&copy.call))
                {
                    self.store.note_authority_answer(call_ref);
                }
                outcome
            }
            Op::Delete => {
                // A delete of another call than the Element's ends that call
                // alone (the store buries it and keeps the Element), so the
                // guard has nothing of it to protect.
                let of_the_element = !is_another_call(
                    self.store.incarnation(call_ref).as_deref(),
                    incarnation.as_deref(),
                );
                if mode == ApplyMode::Forward
                    && of_the_element
                    && self.element_outlives_the_delete(role, &primary, call_ref, answered).await
                {
                    self.metrics.record_repl_forward_flush_refused("delete");
                    return ApplyOutcome::Refused;
                }
                let opts = PutOpts { incarnation, ..PutOpts::default() };
                let _ =
                    self.store.delete_call(role, &primary, call_ref, indexes, false, &opts).await;
                self.metrics.record_repl_applied(self.flow_label(), self.peer_ordinal(), "delete");
                ApplyOutcome::Applied
            }
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

    /// A stored or incoming body decoded, its refusal counted and logged.
    fn decode_counted(&self, body: &[u8]) -> Option<Enveloped> {
        crate::repl::decode_peer_body(&self.codec, &self.metrics, body)
    }

    /// Whether the stored Element records a call this `Delete` must not end: an
    /// Element that is still there (`current_cv` — an EXPIRED one is already gone
    /// and lets the delete land), whose body is **`Active`**, whose caller **was
    /// answered**, and for which the deleting authority has never published an
    /// answer. That is the one fact D3 protects: an answer somebody else gave on
    /// this Element, which the authority is tearing down because it never saw it
    /// (ADR-0031 D3).
    ///
    /// The authority publishes its answer by flushing it
    /// ([`authority_answered`](ReplicatingCallStore::authority_answered)) or by
    /// this `Delete` stating it (`stated_answered`, the frame's `answered`). A
    /// `Delete` stating no answer only leaves the flushed mark to decide.
    ///
    /// Everything else takes delete-wins. An ending Element (`Terminating` /
    /// `Terminated`) goes with the delete — the call is over whoever ended it, and
    /// its record rides the reverse path. An authority that has published the
    /// answer is ending a call it knows about, however far behind its vector and
    /// however long its flushes have been refused.
    ///
    /// The answer never depends on whether this node also holds a LIVE copy: a
    /// takeover copy is ephemeral by construction — it self-releases the moment
    /// its transactions clear, and the Element is then the call's only record.
    /// The read is the **non-evicting**
    /// [`peek_body_raw`](ReplicatingCallStore::peek_body_raw) — the guard must not
    /// destroy the Element it protects — and happens only when the authority is
    /// unanswered, so an ordinary teardown decodes nothing.
    async fn element_outlives_the_delete(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        stated_answered: bool,
    ) -> bool {
        if stated_answered
            || self.store.current_cv(role, primary, call_ref).is_none()
            || self.store.authority_answered(call_ref)
        {
            return false;
        }
        let Some(raw) = self.store.peek_body_raw(role, primary, call_ref).await else {
            return false;
        };
        self.decode_counted(&raw).is_some_and(|copy| {
            copy.call.state == CallModelState::Active && call::helpers::caller_answered(&copy.call)
        })
    }

    /// Hand a frame's outcome up to the router (ADR-0011 X11 / ADR-0014 /
    /// ADR-0055 §7). An applied reverse flush is reclaimed; a refused one, in
    /// either direction, rides up **with its body** to meet the live copy, the
    /// latest of its call in the batch, once the batch is read whole
    /// ([`held_meetings`](super::held_meetings)); any other frame of the call
    /// rides up after it. A bootstrap import signals nothing — the bulk reclaim
    /// covers the whole partition once.
    ///
    /// A pre-bootstrap Backup-flow refusal signals nothing either: the bulk
    /// re-seed replays the whole `bak:{peer}` keyspace, so its refusals are the
    /// state of the two views and not an event, and meeting each one would drive
    /// one meeting per resident call. They are counted; only the tail is handed
    /// up.
    ///
    /// An applied forward write — a `Put` with its body, a `Delete` with the
    /// incarnation it names — rides up only for a call this node serves as a
    /// live takeover copy ([`RouterSink`](crate::repl::RouterSink)), in the scan
    /// and on the tail alike: either way it is a write of the call's primary the
    /// copy has not seen (ADR-0047 D7). An applied reverse `Delete`, with the
    /// incarnation it names, rides up for every call: the backup discharged the
    /// call, and a live copy of it here is dropped (ADR-0047 D8). No other
    /// `Delete` rides up.
    pub(super) fn signal(
        &self,
        mode: ApplyMode,
        outcome: ApplyOutcome,
        bootstrapped: bool,
        call_ref: &str,
        write: PrimaryWrite,
        origin_now_ms: i64,
    ) {
        let Some(sink) = &self.repl_tx else { return };
        let cmd = command(mode, outcome, bootstrapped, call_ref, write, origin_now_ms, sink);
        match cmd {
            Some(
                cmd @ (ReplCommand::ReverseFlushRefused { .. }
                | ReplCommand::ForwardFlushRefused { .. }),
            ) => self.held_meetings.hold(call_ref, cmd),
            other => {
                if let Some(held) = self.held_meetings.take(call_ref) {
                    sink.send(held);
                }
                if let Some(cmd) = other {
                    sink.send(cmd);
                }
            }
        }
    }

    /// Hand the router every refused copy the batch held, the batch read
    /// whole ([`held_meetings`](super::held_meetings)).
    pub(super) fn release_meetings(&self) {
        let held = self.held_meetings.take_all();
        if let Some(sink) = &self.repl_tx {
            for cmd in held {
                sink.send(cmd);
            }
        }
    }
}

/// The command a frame's outcome hands the router ([`Puller::signal`]), or
/// none.
fn command(
    mode: ApplyMode,
    outcome: ApplyOutcome,
    bootstrapped: bool,
    call_ref: &str,
    write: PrimaryWrite,
    origin_now_ms: i64,
    sink: &crate::repl::RouterSink,
) -> Option<ReplCommand> {
    use ApplyOutcome::{Applied, Dominated, Refused, Shed, Superseded};
    if (mode, outcome) == (ApplyMode::Forward, Applied) {
        let call_ref = call_ref.to_string();
        return sink
            .serves_takeover(&call_ref)
            .then_some(ReplCommand::ForwardFlushApplied { call_ref, write });
    }
    let body = match write {
        PrimaryWrite::Put(body) => body,
        PrimaryWrite::Delete(incarnation) => {
            let applied_reverse = mode != ApplyMode::Forward && outcome == Applied;
            let call_ref = call_ref.to_string();
            return applied_reverse
                .then_some(ReplCommand::ReverseDeleteApplied { call_ref, incarnation });
        }
    };
    if mode == ApplyMode::Forward && !bootstrapped {
        return None;
    }
    let call_ref = call_ref.to_string();
    match (mode, outcome) {
        // Reclaim straggler: a backup reverse-flushed one of OUR calls after
        // the bulk sweep — re-materialise + re-serve it.
        (ApplyMode::Reverse, Applied) => Some(ReplCommand::ReclaimCall(call_ref)),
        (ApplyMode::Reverse, Dominated | Refused) => {
            Some(ReplCommand::ReverseFlushRefused { call_ref, body, origin_now_ms })
        }
        // Forward: only a SPLIT rides up. A dominated re-delivery says nothing
        // the live copy does not already hold.
        (ApplyMode::Forward, Refused) => {
            Some(ReplCommand::ForwardFlushRefused { call_ref, body, origin_now_ms })
        }
        (ApplyMode::Forward, Applied | Dominated) | (ApplyMode::Bootstrap, _) => None,
        (_, Shed | Superseded) => None,
    }
}

/// What a stored copy kept over an incoming one is written with.
struct KeptCopy {
    body_ttl_ms: i64,
    incarnation: Option<String>,
}

/// What a `Data` frame wrote, as [`Puller::signal`] hands it up: a `Put`'s
/// body (empty when the frame carries none), a `Delete`'s incarnation.
pub(super) fn written(
    op: Op,
    body: &Option<Arc<[u8]>>,
    incarnation: &Option<String>,
) -> PrimaryWrite {
    match op {
        Op::Put => PrimaryWrite::Put(body.clone().unwrap_or_else(|| Arc::from(Vec::new()))),
        Op::Delete => PrimaryWrite::Delete(incarnation.clone()),
    }
}
