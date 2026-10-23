//! Turn points (ADR-0055 §13): every turn a call takes at its primary, numbered
//! from 1, with its three sub-points — the write handed to replication
//! ([`TurnPoint::Logged`]), the effects left ([`TurnPoint::Left`]) and the write
//! applied at the backup ([`TurnPoint::Applied`]). The cluster's workers report
//! to one [`TurnPoints`]; a test arms one sub-point of one turn and is handed
//! the turn, held there, the instant it is reached.
//!
//! A turn is numbered at the call's primary, the ordinal its `call_ref` names.
//! [`TurnPoint::Applied`] of turn `k` is the first write of the call the backup
//! applies that holds turn `k`'s: a later write covers it. A turn that wrote
//! nothing has no applied sub-point.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use b2bua::turn_probe::{Hold, TurnMark, TurnProbe};
pub use b2bua::TurnPoint;
use tokio::sync::oneshot;

/// One turn of a call at its primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn {
    /// The call's `(primary, backup)` write counters as the turn left them.
    pub version: (i64, i64),
    /// Whether the turn wrote the call to replication.
    pub wrote: bool,
}

impl Turn {
    /// The sub-points this turn has: [`TurnPoint::Applied`] only when it
    /// wrote.
    pub fn points(&self) -> &'static [TurnPoint] {
        if self.wrote {
            &[TurnPoint::Logged, TurnPoint::Left, TurnPoint::Applied]
        } else {
            &[TurnPoint::Logged, TurnPoint::Left]
        }
    }
}

/// A sub-point of one turn: where an incident arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TurnAt {
    /// The turn, numbered from 1.
    pub turn: usize,
    pub point: TurnPoint,
}

impl TurnAt {
    /// The sub-point's name in a cell id: `t3-left`.
    pub fn label(&self) -> String {
        format!("t{}-{}", self.turn, self.point.label())
    }
}

/// The armed turn, held at its sub-point. Dropped without
/// [`release`](Self::release), the turn never goes on: the node stands frozen
/// at the sub-point, as a killed process does.
pub struct HeldTurn {
    /// The node holding the turn.
    pub node: String,
    pub call_ref: String,
    release: oneshot::Sender<()>,
}

impl HeldTurn {
    /// Let the turn go on.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}

/// The turn points of every call the cluster serves. Installed on every worker
/// of a [`FailoverHarness`](crate::FailoverHarness).
#[derive(Default)]
pub struct TurnPoints {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Every call's turns at its primary, by `call_ref`, in the order calls
    /// took their first turn.
    calls: BTreeMap<String, Vec<Turn>>,
    order: Vec<String>,
    armed: Option<Armed>,
}

struct Armed {
    at: TurnAt,
    /// The call armed; `None` arms the first call that takes a turn.
    call_ref: Option<String>,
    reached: oneshot::Sender<HeldTurn>,
}

impl TurnPoints {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The turns `call_ref` took at its primary so far.
    pub fn turns(&self, call_ref: &str) -> Vec<Turn> {
        self.lock().calls.get(call_ref).cloned().unwrap_or_default()
    }

    /// The first call that took a turn, with its turns so far.
    pub fn first_call(&self) -> Option<(String, Vec<Turn>)> {
        let inner = self.lock();
        let call_ref = inner.order.first()?.clone();
        let turns = inner.calls.get(&call_ref).cloned().unwrap_or_default();
        Some((call_ref, turns))
    }

    /// Arm `at` on the first call that takes a turn; the returned receiver
    /// yields the turn held there once it is reached. Replaces any arm.
    pub fn arm(&self, at: TurnAt) -> oneshot::Receiver<HeldTurn> {
        let (reached, rx) = oneshot::channel();
        self.lock().armed = Some(Armed { at, call_ref: None, reached });
        rx
    }

    /// [`arm`](Self::arm) on the call `call_ref`.
    pub fn arm_call(&self, call_ref: &str, at: TurnAt) -> oneshot::Receiver<HeldTurn> {
        let (reached, rx) = oneshot::channel();
        self.lock().armed = Some(Armed { at, call_ref: Some(call_ref.to_string()), reached });
        rx
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Inner {
    /// Record `mark` and say which turn of its call it stands at.
    fn record(&mut self, mark: &TurnMark<'_>) -> Option<usize> {
        let primary = call::call_ref_primary(mark.call_ref)?;
        let at_primary = primary == mark.node;
        match (mark.point, at_primary) {
            (TurnPoint::Logged, true) => {
                if !self.calls.contains_key(mark.call_ref) {
                    self.order.push(mark.call_ref.to_string());
                }
                let turns = self.calls.entry(mark.call_ref.to_string()).or_default();
                turns.push(Turn { version: mark.version, wrote: mark.wrote });
                Some(turns.len())
            }
            (TurnPoint::Left, true) => self.calls.get(mark.call_ref).map(Vec::len),
            (TurnPoint::Applied, false) => {
                let turns = self.calls.get(mark.call_ref)?;
                // The earliest written turn this write covers and not yet
                // applied: the armed turn's own check reads the arm below.
                turns.iter().position(|t| t.wrote && covers(mark, t)).map(|i| i + 1)
            }
            _ => None,
        }
    }

    /// Whether `mark` reaches the armed sub-point: the armed call, the armed
    /// point, and the armed turn (for an applied write, one that covers it).
    fn reaches(&self, mark: &TurnMark<'_>, turn: usize) -> bool {
        let Some(armed) = &self.armed else {
            return false;
        };
        let call_ok = match &armed.call_ref {
            Some(c) => c == mark.call_ref,
            None => self.order.first().is_some_and(|c| c == mark.call_ref),
        };
        if !call_ok || armed.at.point != mark.point {
            return false;
        }
        match mark.point {
            TurnPoint::Applied => self
                .calls
                .get(mark.call_ref)
                .and_then(|turns| turns.get(armed.at.turn.checked_sub(1)?))
                .is_some_and(|t| t.wrote && covers(mark, t)),
            _ => armed.at.turn == turn,
        }
    }
}

/// Whether the applied write `mark` holds the write of `turn`.
fn covers(mark: &TurnMark<'_>, turn: &Turn) -> bool {
    mark.removed || mark.version.0 >= turn.version.0
}

impl TurnProbe for TurnPoints {
    fn reached(&self, mark: &TurnMark<'_>) -> Option<Hold> {
        let mut inner = self.lock();
        let turn = inner.record(mark)?;
        if !inner.reaches(mark, turn) {
            return None;
        }
        let armed = inner.armed.take()?;
        let (release, held) = oneshot::channel::<()>();
        let turn =
            HeldTurn { node: mark.node.to_string(), call_ref: mark.call_ref.to_string(), release };
        // Nobody waiting: the turn goes on.
        if armed.reached.send(turn).is_err() {
            return None;
        }
        Some(Box::pin(async move {
            if held.await.is_err() {
                std::future::pending::<()>().await;
            }
        }))
    }
}
