//! The backup-deferral gate (ADR-0014 boot order): the Backup streams open
//! once the Reclaim flows of the ready members are current or unreachable.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use topology::{Membership, Peer};

use super::{fold, seen, ReplicationSupervisor, BACKUP_GATE_POLL};
use crate::repl::puller::PullerStatus;

impl ReplicationSupervisor {
    /// Wait until the Reclaim flows for every desired peer are ready, then latch
    /// `backup_enabled` and reconcile once so the Backup streams open. Exits
    /// immediately for a peerless node (nothing to back up). One-shot: returns
    /// once it has enabled backups; later peers get their Backup flow from the
    /// periodic reconcile (which honours the latched flag).
    ///
    /// **Event-driven, not a clock-poll.** It wakes the instant any Reclaim
    /// puller publishes a status change (its first `Noop` flips `current`), so the
    /// Backup streams open within the same scheduler turn — no full poll interval
    /// of latency. A `BACKUP_GATE_POLL` fallback bounds the wait against a lost
    /// wakeup or an unreachable peer whose hard timer must still latch it.
    pub(super) async fn run_backup_gate(self, membership: Arc<dyn Membership>) {
        loop {
            if self.inner.backup_enabled.load(Ordering::SeqCst) {
                return;
            }
            let desired: Vec<Peer> = membership
                .snapshot()
                .into_iter()
                .filter(|p| p.ordinal != self.inner.self_ordinal)
                .collect();
            // A peerless node has no Backup streams to open — latch and stop.
            if desired.is_empty() {
                self.inner.backup_enabled.store(true, Ordering::SeqCst);
                return;
            }
            self.sync();
            // Only a ready member gates the latch (`desired_ordinals`): a member
            // that is present but not ready is pulled, never waited on. Each
            // status is marked seen before it is read, under the one lock, so
            // the wait below wakes on every status published after the mark.
            let (ready, receivers) = {
                let mut peers = self.inner.peers();
                let receivers: Vec<watch::Receiver<PullerStatus>> = desired
                    .iter()
                    .filter_map(|p| peers.get(&p.ordinal).and_then(|e| seen(&e.reclaim)))
                    .collect();
                fold(&mut peers);
                let ready = desired
                    .iter()
                    .filter(|p| p.ready)
                    .all(|p| peers.get(&p.ordinal).is_some_and(|e| e.reclaim.ready()));
                (ready, receivers)
            };
            if ready {
                self.inner.backup_enabled.store(true, Ordering::SeqCst);
                self.reconcile_from_snapshot(membership.snapshot());
                return;
            }
            // Wait for ANY Reclaim status change (readiness re-check) or the
            // fallback poll, whichever comes first.
            await_any_change_or_poll(receivers, BACKUP_GATE_POLL).await;
        }
    }
}

/// Wait until **any** of `receivers` publishes a status change, or `fallback`
/// elapses — whichever first. Used by the backup-deferral gate so it re-checks
/// Reclaim readiness the instant a puller advances, without a full poll interval
/// of latency. Each receiver must be marked seen at the status the caller read
/// ([`seen`]). A receiver whose puller has ended never wakes it, so once none can
/// publish the wait is the `fallback` poll. A short-lived forwarder task per
/// receiver (aborted on return) avoids needing a `select_all` combinator; the
/// gate is one-shot, so this runs only during boot until the Backup streams open.
async fn await_any_change_or_poll(
    receivers: Vec<watch::Receiver<PullerStatus>>,
    fallback: Duration,
) {
    if receivers.is_empty() {
        tokio::time::sleep(fallback).await;
        return;
    }
    #[expect(clippy::disallowed_methods, reason = "a capacity of one")]
    let (tx, mut rx) = mpsc::channel::<()>(1);
    let handles: Vec<tokio::task::JoinHandle<()>> = receivers
        .into_iter()
        .map(|mut r| {
            let tx = tx.clone();
            observe::spawn_keeping_node(async move {
                if r.changed().await.is_ok() {
                    let _ = tx.try_send(());
                }
            })
        })
        .collect();
    drop(tx);
    // `None` (every forwarder ended without a change) disables its branch.
    tokio::select! {
        _ = tokio::time::sleep(fallback) => {}
        Some(()) = rx.recv() => {}
    }
    for h in handles {
        h.abort();
    }
}
