//! The state a supervisor retains per peer: one [`FlowState`] per flow, kept
//! across Park, disconnect and re-add, and the readiness each flow and the
//! peer as a whole read from it (ADR-0011 X5).

use repl_net::frame::{Partition, Watermark};
use tokio::sync::watch;

use crate::repl::puller::{ConnectNow, PullerStatus};

/// Per-`(ordinal, flow)` retained replication state — survives Park/disconnect/
/// re-add. One of these per flow lives inside a [`PeerEntry`].
pub(super) struct FlowState {
    /// The running puller's status receiver (current flag + live watermark), or
    /// `None` while Parked.
    pub(super) status_rx: Option<watch::Receiver<PullerStatus>>,
    /// Cancel handle to interrupt the running puller (Park / re-spawn).
    pub(super) cancel_tx: Option<watch::Sender<bool>>,
    /// Asks the running puller for one connect at once, or `None` while Parked.
    pub(super) connect_now: Option<ConnectNow>,
    /// The retained watermark — authoritative, survives the puller task. Seeded
    /// into the next puller on re-spawn.
    pub(super) watermark: Watermark,
    /// Sticky current flag — retained across Park (never cleared, ADR-0011 X6).
    pub(super) current: bool,
    /// Sticky bootstrap-complete flag (X5) — retained across Park. Set when the
    /// puller hit the first catch-up `Noop`, the hard timer fired, or it resumed
    /// warm.
    pub(super) bootstrap_complete: bool,
    /// Highest reset generation absorbed from any puller for this flow. A puller
    /// bumps its `reset_gen` when the server pushes `ResetToBootstrap` (watermark
    /// forced back to `(0,0)`); a higher value here means we must pull the
    /// retained watermark DOWN so a respawn re-bootstraps instead of resuming the
    /// now-invalid high W.
    pub(super) reset_gen: u64,
    /// Sticky: this flow was reached by a successful `connect` at least once. An
    /// unreachable peer (never connected) that goes bootstrap-complete only by the
    /// hard timer must NOT pin readiness NotReady (ADR-0011 X5 — liveness).
    pub(super) ever_connected: bool,
}

impl FlowState {
    /// Whether the flow's puller has ended while it was neither parked nor
    /// replaced: a puller runs until it is cancelled, so only a panic ends
    /// one the supervisor still holds the status of.
    pub(super) fn puller_ended(&self) -> bool {
        self.status_rx.as_ref().is_some_and(|rx| rx.has_changed().is_err())
    }

    pub(super) fn cold() -> Self {
        Self {
            status_rx: None,
            cancel_tx: None,
            connect_now: None,
            watermark: Watermark::new(0, 0),
            current: false,
            bootstrap_complete: false,
            reset_gen: 0,
            ever_connected: false,
        }
    }

    /// Fold the puller's latest published status into the retained copy. The
    /// current and ever-connected flags are sticky (OR). A `reset_gen` advance means the server
    /// forced a re-bootstrap: pull the watermark DOWN to `(0,0)` and clear
    /// bootstrap-complete so a respawn re-bootstraps. Otherwise bootstrap-complete
    /// is sticky and the watermark only advances.
    pub(super) fn absorb(&mut self) {
        if let Some(rx) = &self.status_rx {
            let s = *rx.borrow();
            self.current |= s.current;
            self.ever_connected |= s.ever_connected;
            if s.reset_gen > self.reset_gen {
                self.reset_gen = s.reset_gen;
                self.watermark = s.watermark; // (0,0)
                self.bootstrap_complete = s.bootstrap_complete; // false
            } else {
                self.bootstrap_complete |= s.bootstrap_complete;
                if s.watermark > self.watermark {
                    self.watermark = s.watermark;
                }
            }
        }
    }

    /// True iff this flow is current OR was never reachable and best-effort
    /// completed (the readiness predicate, ADR-0011 X5). A reachable-then-blipped
    /// flow keeps the strict gate (sticky `current`).
    pub(super) fn ready(&self) -> bool {
        self.current || (self.bootstrap_complete && !self.ever_connected)
    }
}

/// Per-ordinal retained state: one [`FlowState`] per flow + the last binding.
pub(super) struct PeerEntry {
    /// Reclaim flow (`partition = Pri`) — drives readiness + reclaim.
    pub(super) reclaim: FlowState,
    /// Backup flow (`partition = Bak`) — the peer's calls this node backs up;
    /// opened once the Reclaim flows are ready, gates readiness.
    pub(super) backup: FlowState,
    /// The membership binding — `host` and worker identity — the running
    /// pullers were last spawned for. The drift signal for the periodic
    /// snapshot reconcile (ADR-0012 D2): a desired peer whose host moved or
    /// whose identity changed (ADR-0055 §6), or whose flows are not running,
    /// is (re)spawned. `None` until a puller has been spawned for this ordinal.
    pub(super) binding: Option<(String, Option<String>)>,
    /// The peer's `ready` condition as last reconciled. Not a drift signal — a
    /// condition flip never respawns a flow — only what [`PeerLink::Kept`], the
    /// transition log and the pulled-not-ready gauge read.
    pub(super) ready: bool,
}

impl PeerEntry {
    pub(super) fn cold() -> Self {
        Self { reclaim: FlowState::cold(), backup: FlowState::cold(), binding: None, ready: true }
    }

    /// Whether this peer's flows let the node read ready: the Reclaim flow is
    /// [`ready`](FlowState::ready), and the Backup flow is current, or was
    /// never reached and its own hard timer or the Reclaim flow's released it
    /// (ADR-0011 X5): a peer unreachable since the boot holds readiness no
    /// longer than the Reclaim flow's hard timer.
    pub(super) fn ready(&self) -> bool {
        let reclaim_unreached = self.reclaim.bootstrap_complete && !self.reclaim.ever_connected;
        let backup = &self.backup;
        self.reclaim.ready()
            && (backup.current
                || (!backup.ever_connected && (backup.bootstrap_complete || reclaim_unreached)))
    }

    pub(super) fn flow(&self, partition: Partition) -> &FlowState {
        match partition {
            Partition::Pri => &self.reclaim,
            Partition::Bak => &self.backup,
        }
    }

    /// Park BOTH flows: fold progress, interrupt, drop the status receivers. The
    /// retained watermarks/current flags survive.
    pub(super) fn park(&mut self) {
        for flow in [&mut self.reclaim, &mut self.backup] {
            flow.absorb();
            if let Some(tx) = flow.cancel_tx.take() {
                let _ = tx.send(true);
            }
            flow.status_rx = None;
            flow.connect_now = None;
        }
    }
}
