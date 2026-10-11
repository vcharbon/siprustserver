//! How a [`Puller`] reconciles an inbound `Data` frame into the local store
//! under the ADR-0014 `(p,b)` rule, and what it hands up to the router.

use std::sync::Arc;

use call::{CallBodyCodec, CallModelState};
use repl_net::frame::{Op, Partition};

use super::super::incarnation::is_another_call;
use super::Puller;
use crate::router::ReplCommand;
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
    /// Backup flow — a primary→backup update. The follower defers to the
    /// authority, but never regresses progress an acting backup authored on the
    /// Element itself (ADR-0031 D3).
    Forward,
    /// Reclaim-flow steady tail — apply the ADR-0014 Reverse `(p,b)` rule.
    Reverse,
}

/// What [`Puller::apply_to_store`] did with a `Data` frame. `Dominated` and
/// `Refused` both leave the store untouched but mean opposite things: a
/// dominated frame is a re-delivery of a version already held, a refused one is
/// a version the local copy will not take — the two views have split, and only
/// that case is counted and handed to the fold (ADR-0031 D3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ApplyOutcome {
    Applied,
    Dominated,
    Refused,
    /// A backup replica of a call not held yet, left unstored at a backup
    /// ceiling (ADR-0037). Says nothing about the call; the next `Put` that
    /// finds room stores it.
    Shed,
    /// A reverse flush of a call incarnation other than the Element's: a
    /// takeover copy's view of a call the authority has since replaced on the
    /// ref. The Element keeps its call and nothing is handed to the fold.
    Superseded,
}

impl Puller {
    /// Apply a `Data` frame's mutation to the local store under the **ADR-0014
    /// `(p,b)` version-vector rule**, **without** touching the watermark.
    ///
    /// `partition = Bak` → store as `bak:{primary}` (a **Forward** primary→backup
    /// update); `partition = Pri` → store as `pri:{primary}` (a **Reverse**
    /// backup→primary reverse-flush, or a bootstrap import).
    ///
    /// Apply decision (`(call_gen, call_bgen)` = incoming `(p,b)`; `(sp, sb)` =
    /// stored):
    ///
    /// | mode | `Put` | `Delete` |
    /// |---|---|---|
    /// | [`Bootstrap`](ApplyMode::Bootstrap) | apply unless `(sp, sb)` dominates | apply |
    /// | [`Forward`](ApplyMode::Forward) | apply unless `(sp, sb)` dominates, refuse when the body branches off the Element or `sb > b_in` | apply unless the Element is a live call the authority never answered (by a flush or by this `Delete`) |
    /// | [`Reverse`](ApplyMode::Reverse) | apply iff `p_in == sp && b_in > sb` | apply |
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
    /// Bootstrap recovery and the Reverse tail keep delete-wins outright. Forward
    /// is the one direction that yields (ADR-0031 D3): a `Put` whose body stands
    /// behind the Element on the call's lifecycle, or whose `b_in` is behind the
    /// Element's own `b`, is a branch of the call the Element records and is
    /// refused; a `Delete` is refused only for the one fact the guard protects —
    /// an ACTIVE, answered Element the authority has never published an answer
    /// for, by a flush or by the `Delete` itself. Both Forward refusals count into
    /// `b2bua_repl_forward_flush_refused_total{op}`.
    ///
    /// Returns the [`ApplyOutcome`]: a dominated re-delivery and a refusal both
    /// leave the store untouched, and only the refusal is a split of the two
    /// views. The reclaim tail reclaims on an applied reverse flush and hands a
    /// refused flush up with its body, so the call model can fold lifecycle
    /// progress the vector cannot see.
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
                // The Forward direction reads the frame's body twice — for the
                // branch compare and for the authority's view of the answer — so
                // it is decoded once, here.
                let incoming = (mode == ApplyMode::Forward)
                    .then(|| body.as_deref().and_then(|b| self.codec.decode(b).ok()))
                    .flatten();
                // Stored `(sp,sb)` DOMINATES incoming `(p,b)` ⇒ skip (idempotent /
                // reordered re-delivery). The dominance gate is the ONLY
                // idempotency (no watermark apply-gate): it is what makes every
                // bootstrap frame — all sharing `at = W` — safe to apply.
                let dominated = |sp: i64, sb: i64| sp >= call_gen && sb >= call_bgen;
                let outcome = match mode {
                    // Bootstrap recovery: the authority's body, monotone by `(p,b)`.
                    ApplyMode::Bootstrap => match stored {
                        Some((sp, sb)) if !another_call && dominated(sp, sb) => {
                            ApplyOutcome::Dominated
                        }
                        _ => ApplyOutcome::Applied,
                    },
                    // Forward (primary → backup): the D3 guard, counters first and
                    // then the bodies.
                    ApplyMode::Forward if another_call => ApplyOutcome::Applied,
                    ApplyMode::Forward => {
                        self.forward_put_outcome(
                            role,
                            &primary,
                            call_ref,
                            stored,
                            (call_gen, call_bgen),
                            incoming.as_ref(),
                        )
                        .await
                    }
                    // Reverse (backup → primary): accept iff untouched-by-us since
                    // the backup branched (`p_in == sp`) AND a genuinely newer
                    // backup mutation (`b_in > sb`); no local copy → accept (the
                    // reactive reclaim materialises it). Else keep our own — and
                    // hand it up, re-delivery included: the fold reads the body for
                    // progress the vector cannot carry (ADR-0014 amendment).
                    ApplyMode::Reverse if another_call => ApplyOutcome::Superseded,
                    ApplyMode::Reverse => match stored {
                        Some((sp, sb)) if call_gen != sp || call_bgen <= sb => {
                            ApplyOutcome::Refused
                        }
                        _ => ApplyOutcome::Applied,
                    },
                };
                let outcome = if outcome == ApplyOutcome::Applied
                    && stored.is_none()
                    && role == PartitionRole::Backup
                    && !self.store.buries(call_ref, incarnation.as_deref())
                    && self
                        .capacity
                        .as_ref()
                        .and_then(|g| g.refuses_backup(self.store.backup_held()))
                        .is_some()
                {
                    ApplyOutcome::Shed
                } else {
                    outcome
                };
                if outcome == ApplyOutcome::Refused && mode == ApplyMode::Forward {
                    self.metrics.record_repl_forward_flush_refused("put");
                }
                if outcome == ApplyOutcome::Applied {
                    let body = body.map(|b| b.to_vec()).unwrap_or_default();
                    let _ = self
                        .store
                        .put_call(
                            role,
                            &primary,
                            call_ref,
                            body,
                            indexes,
                            body_ttl_ms,
                            call_gen,
                            call_bgen,
                            // Apply locally only — do NOT re-propagate (peer:None).
                            // Carry the origin wall clock so the store can persist
                            // the receive-time skew offset for later timer
                            // re-anchoring on failover/reclaim (clock-skew hardening).
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
                if incoming.as_ref().is_some_and(call::helpers::caller_answered) {
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

    /// The Forward `Put` decision (ADR-0031 D3), in three checks:
    ///
    /// 1. `(sp, sb)` dominates the frame ⇒ a re-delivery of a version already
    ///    held. Not a split; nothing to report.
    /// 2. `sb > 0` (an acting backup authored this Element) and the frame's body
    ///    stands behind the Element on either lifecycle axis
    ///    (`call::helpers::lifecycle_regresses`) ⇒ the authority is serving a
    ///    BRANCH of this call. Refused. This is the one read that touches the
    ///    Element; `sb == 0` skips it.
    /// 3. `sb > b_in` ⇒ the frame names a version behind the one the Element
    ///    carries. Refused: whatever its body says, its sender has not seen what
    ///    an acting backup wrote here.
    async fn forward_put_outcome(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        stored: Option<(i64, i64)>,
        version: (i64, i64),
        incoming: Option<&call::Call>,
    ) -> ApplyOutcome {
        let (call_gen, call_bgen) = version;
        let Some((sp, sb)) = stored else {
            return ApplyOutcome::Applied;
        };
        if sp >= call_gen && sb >= call_bgen {
            return ApplyOutcome::Dominated;
        }
        if sb > 0 && self.regresses_the_element(role, primary, call_ref, incoming).await {
            return ApplyOutcome::Refused;
        }
        if sb > call_bgen {
            return ApplyOutcome::Refused;
        }
        ApplyOutcome::Applied
    }

    /// Whether the flushed body stands behind the stored Element on either
    /// lifecycle axis. The Element is read with the **non-evicting**
    /// [`peek_body_raw`](ReplicatingCallStore::peek_body_raw), so the compare
    /// never destroys it. An undecodable or absent body on either side answers
    /// `false` — the counters stay the only gate where the model cannot speak.
    async fn regresses_the_element(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        incoming: Option<&call::Call>,
    ) -> bool {
        let Some(incoming) = incoming else {
            return false;
        };
        let Some(raw) = self.store.peek_body_raw(role, primary, call_ref).await else {
            return false;
        };
        self.codec
            .decode(&raw)
            .is_ok_and(|held| call::helpers::lifecycle_regresses(incoming, &held))
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
        self.codec.decode(&raw).is_ok_and(|call| {
            call.state == CallModelState::Active && call::helpers::caller_answered(&call)
        })
    }

    /// Hand a `Put`'s outcome up to the router (ADR-0011 X11 / ADR-0014 /
    /// ADR-0031 D3). An applied reverse flush is reclaimed; a refused one, in
    /// either direction, rides up **with its body**, because the call model can
    /// read lifecycle progress in it that the counters cannot carry. A bootstrap
    /// import signals nothing — the bulk reclaim covers the whole partition once.
    ///
    /// A pre-bootstrap Backup-flow frame signals nothing either: the bulk re-seed
    /// replays the whole `bak:{peer}` keyspace, so its refusals are the state of
    /// the two views and not an event, and folding each one would drive one fold
    /// per resident call. They are counted; only the tail is handed up.
    pub(super) fn signal_put_outcome(
        &self,
        mode: ApplyMode,
        outcome: ApplyOutcome,
        bootstrapped: bool,
        call_ref: &str,
        body: Option<Arc<[u8]>>,
        origin_now_ms: i64,
    ) {
        use ApplyOutcome::{Applied, Dominated, Refused, Shed, Superseded};
        let Some(tx) = &self.repl_tx else { return };
        if mode == ApplyMode::Forward && !bootstrapped {
            return;
        }
        let refused_body = || body.clone().unwrap_or_else(|| Arc::from(Vec::new()));
        let cmd = match (mode, outcome) {
            // Reclaim straggler: a backup reverse-flushed one of OUR calls after
            // the bulk sweep — re-materialise + re-serve it.
            (ApplyMode::Reverse, Applied) => ReplCommand::ReclaimCall(call_ref.to_string()),
            (ApplyMode::Reverse, Dominated | Refused) => ReplCommand::ReverseFlushRefused {
                call_ref: call_ref.to_string(),
                body: refused_body(),
                origin_now_ms,
            },
            // Forward: only a SPLIT rides up. A dominated re-delivery says nothing
            // the live copy does not already hold.
            (ApplyMode::Forward, Refused) => ReplCommand::ForwardFlushRefused {
                call_ref: call_ref.to_string(),
                body: refused_body(),
                origin_now_ms,
            },
            (ApplyMode::Forward, Applied | Dominated) | (ApplyMode::Bootstrap, _) => return,
            (_, Shed | Superseded) => return,
        };
        let _ = tx.send(cmd);
    }
}
