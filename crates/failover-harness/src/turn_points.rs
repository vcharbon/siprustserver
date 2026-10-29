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
//! nothing has no applied sub-point. [`TurnPoint::Left`] is reported by the
//! call's outbox, after later turns may have been taken: it names the turn by
//! the version that turn logged.
//!
//! Each turn records why its outbox entry left ([`Released`]). A cap release
//! while the harness holds an applied sub-point of the call is the harness's
//! own doing: the backup's position report waits behind that hold.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use b2bua::turn_probe::{Hold, TurnMark, TurnProbe};
pub use b2bua::{Released, TurnPoint};
use tokio::sync::oneshot;

/// One turn of a call at its primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn {
    /// The call's `(primary, backup)` write counters as the turn left them.
    pub version: (i64, i64),
    /// Whether the turn wrote the call to replication.
    pub wrote: bool,
    /// Whether the turn handled a SIP request rather than a response, a timer
    /// or an off-turn answer.
    pub on_request: bool,
    /// Whether the turn was taken on an outside service's answer, or on its
    /// deadline's expiry (ADR-0039).
    pub on_answer: bool,
    /// Whether the turn sent a request to an outside service.
    pub called_out: bool,
    /// Why the turn's outbox entry left, once it did.
    pub released: Option<Released>,
    /// The entry left at the cap while the harness held an applied sub-point
    /// of the call.
    pub capped_by_harness: bool,
    /// The backup applied a write holding this turn's.
    pub applied: bool,
    /// The call had a live, bootstrapped backup when the turn logged its
    /// write.
    pub backed: bool,
}

impl Turn {
    /// The sub-points this turn has, in the order the `applied` sync level
    /// reaches them: [`TurnPoint::Applied`] only when it wrote.
    pub fn points(&self) -> &'static [TurnPoint] {
        if self.wrote {
            &[TurnPoint::Logged, TurnPoint::Applied, TurnPoint::Left]
        } else {
            &[TurnPoint::Logged, TurnPoint::Left]
        }
    }

    /// The turn's outbox entry left at the cap of its own accord: not because
    /// the backup left, nor while the harness held an applied sub-point.
    pub fn cap_hit(&self) -> bool {
        self.released == Some(Released::Capped) && !self.capped_by_harness
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
    /// The calls whose applied sub-point the harness holds now, with how many
    /// holds.
    applied_held: Arc<Mutex<BTreeMap<String, usize>>>,
}

#[derive(Default)]
struct Inner {
    /// Every call's turns at its primary, by `call_ref`, in the order calls
    /// took their first turn.
    calls: BTreeMap<String, Vec<Turn>>,
    order: Vec<String>,
    armed: Option<Armed>,
    /// A node sent again requests another had sent off a call's turn.
    resent: bool,
    /// A node asked again a call's routing decision another had asked.
    reasked: bool,
}

/// An arm on the first call that takes a turn.
struct Armed {
    at: TurnAt,
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

    /// Every call's turns at its primary so far, in the order the calls took
    /// their first turn.
    pub fn calls(&self) -> Vec<(String, Vec<Turn>)> {
        let inner = self.lock();
        let turns = |c: &String| inner.calls.get(c).cloned().unwrap_or_default();
        inner.order.iter().map(|c| (c.clone(), turns(c))).collect()
    }

    /// The first call that took a turn, with its turns so far.
    pub fn first_call(&self) -> Option<(String, Vec<Turn>)> {
        let inner = self.lock();
        let call_ref = inner.order.first()?.clone();
        let turns = inner.calls.get(&call_ref).cloned().unwrap_or_default();
        Some((call_ref, turns))
    }

    /// Whether a node sent again, this run, requests another node had sent
    /// off a call's turn (ADR-0039 decision 5).
    pub fn resent(&self) -> bool {
        self.lock().resent
    }

    /// Whether a node asked again, this run, a call's routing decision another
    /// node had asked (ADR-0047 D4).
    pub fn reasked(&self) -> bool {
        self.lock().reasked
    }

    /// Arm `at` on the first call that takes a turn; the returned receiver
    /// yields the turn held there once it is reached. Replaces any arm.
    pub fn arm(&self, at: TurnAt) -> oneshot::Receiver<HeldTurn> {
        let (reached, rx) = oneshot::channel();
        self.lock().armed = Some(Armed { at, reached });
        rx
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Inner {
    /// Record `mark` and say which turn of its call it stands at.
    fn record(&mut self, mark: &TurnMark<'_>, applied_held: bool) -> Option<usize> {
        self.resent |= mark.resent;
        self.reasked |= mark.reasked;
        let primary = call::call_ref_primary(mark.call_ref)?;
        let at_primary = primary == mark.node;
        match (mark.point, at_primary) {
            (TurnPoint::Logged, true) => {
                if !self.calls.contains_key(mark.call_ref) {
                    self.order.push(mark.call_ref.to_string());
                }
                let turns = self.calls.entry(mark.call_ref.to_string()).or_default();
                let (version, wrote, on_request) = (mark.version, mark.wrote, mark.on_request);
                let (on_answer, called_out) = (mark.on_answer, mark.called_out);
                let (released, capped_by_harness, applied) = (None, false, false);
                turns.push(Turn {
                    version,
                    wrote,
                    on_request,
                    on_answer,
                    called_out,
                    released,
                    capped_by_harness,
                    applied,
                    backed: mark.backed,
                });
                Some(turns.len())
            }
            (TurnPoint::Left, true) => {
                let turns = self.calls.get_mut(mark.call_ref)?;
                // The earliest turn at this version not left yet: entries
                // leave in the order their turns queued them.
                let i =
                    turns.iter().position(|t| t.released.is_none() && t.version == mark.version)?;
                let turn = &mut turns[i];
                turn.released = mark.released;
                turn.capped_by_harness = applied_held && mark.released == Some(Released::Capped);
                Some(i + 1)
            }
            (TurnPoint::Applied, false) => {
                let turns = self.calls.get_mut(mark.call_ref)?;
                let mut first = None;
                for (i, t) in turns.iter_mut().enumerate().filter(|(_, t)| t.wrote) {
                    if covers(mark, t) {
                        t.applied = true;
                        first.get_or_insert(i + 1);
                    }
                }
                // The earliest written turn this write covers: the armed
                // turn's own check reads the arm below.
                first
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
        let call_ok = self.order.first().is_some_and(|c| c == mark.call_ref);
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
        let applied_held = locked(&self.applied_held).contains_key(mark.call_ref);
        let mut inner = self.lock();
        let turn = inner.record(mark, applied_held)?;
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
        drop(inner);
        let applied = (mark.point == TurnPoint::Applied).then(|| mark.call_ref.to_string());
        if let Some(call_ref) = &applied {
            *locked(&self.applied_held).entry(call_ref.clone()).or_default() += 1;
        }
        let applied_held = self.applied_held.clone();
        Some(Box::pin(async move {
            if held.await.is_err() {
                std::future::pending::<()>().await;
            }
            if let Some(call_ref) = applied {
                let mut held = locked(&applied_held);
                if let Some(n) = held.get_mut(&call_ref) {
                    *n -= 1;
                    if *n == 0 {
                        held.remove(&call_ref);
                    }
                }
            }
        }))
    }
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
