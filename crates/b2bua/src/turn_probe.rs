//! The turn-point seam (ADR-0055 §13): a test observes every turn of a call at
//! its sub-points and may hold the turn there, so an incident lands at an exact
//! instant of the turn. A worker built without a probe — production — tests one
//! `Option` per sub-point and builds no mark.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A sub-point of a turn. Under the `applied` sync level a turn whose write
/// the backup applied before the cap runs `Logged → Applied → Left`; under
/// `off`, or past the cap, `Logged → Left → Applied` (ADR-0055 §13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TurnPoint {
    /// The turn persisted the call and handed its write to replication (a turn
    /// that writes nothing stands here too); none of its effects has left.
    Logged,
    /// The turn's last outbox entry left: its effects are handed to the
    /// transport and to the outside services.
    Left,
    /// A backup applied a primary's write of the call.
    Applied,
}

/// Why an outbox entry left (ADR-0055 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Released {
    /// It waited for nothing: the `off` level, or a call with no live,
    /// bootstrapped backup.
    AtOnce,
    /// Its backup applied the write that records it.
    Applied,
    /// The cap expired first.
    Capped,
    /// The flow to its backup went down first.
    BackupLost,
    /// The call was released here (a stand-down, a self-release, a discharge
    /// by the backup): its copy's write records the effects.
    CallReleased,
}

impl TurnPoint {
    /// The point's name in a cell id.
    pub fn label(self) -> &'static str {
        match self {
            TurnPoint::Logged => "logged",
            TurnPoint::Left => "left",
            TurnPoint::Applied => "applied",
        }
    }
}

/// One node reaching one sub-point for one call.
#[derive(Clone, Debug)]
pub struct TurnMark<'a> {
    /// The ordinal of the node at the sub-point.
    pub node: &'a str,
    pub call_ref: &'a str,
    pub point: TurnPoint,
    /// The call's `(primary, backup)` write counters: as the turn left them
    /// ([`Logged`](TurnPoint::Logged), [`Left`](TurnPoint::Left)), or as the
    /// applied write carries them ([`Applied`](TurnPoint::Applied)).
    pub version: (i64, i64),
    /// Whether the turn wrote the call to replication; an applied write is one.
    pub wrote: bool,
    /// The applied write removes the call.
    pub removed: bool,
    /// The turn handled a SIP request, not a response, a timer or an
    /// off-turn answer.
    pub on_request: bool,
    /// The turn sent a request to an outside service, whose answer a later
    /// turn takes.
    pub called_out: bool,
    /// After the turn the call awaits the answer of a request sent off its
    /// turn: an answer deadline is in its ledger (ADR-0039).
    pub awaits_answer: bool,
    /// Why the turn's last outbox entry left: set on [`Left`](TurnPoint::Left)
    /// only.
    pub released: Option<Released>,
}

/// What a probe hands back to hold a turn: the turn goes on once it resolves.
/// A hold that never resolves freezes the task at the sub-point, as a killed
/// process stands.
pub type Hold = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The observer a test installs.
pub trait TurnProbe: Send + Sync {
    /// Called at every sub-point of every turn; `Some` holds the turn there.
    fn reached(&self, mark: &TurnMark<'_>) -> Option<Hold>;
}

/// The handle a worker carries: no probe by default.
#[derive(Clone, Default)]
pub struct TurnProbes(Option<Arc<dyn TurnProbe>>);

impl TurnProbes {
    /// A handle reporting to `probe`.
    pub fn new(probe: Arc<dyn TurnProbe>) -> Self {
        Self(Some(probe))
    }

    /// Report the sub-point `mark` builds and wait out the hold, if any. The
    /// mark is built only when a probe is installed.
    pub(crate) async fn at<'a>(&self, mark: impl FnOnce() -> TurnMark<'a>) {
        let Some(probe) = &self.0 else {
            return;
        };
        if let Some(hold) = probe.reached(&mark()) {
            hold.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Seen(Mutex<Vec<(TurnPoint, (i64, i64))>>);

    impl TurnProbe for Seen {
        fn reached(&self, mark: &TurnMark<'_>) -> Option<Hold> {
            self.0.lock().unwrap().push((mark.point, mark.version));
            None
        }
    }

    fn mark(point: TurnPoint) -> TurnMark<'static> {
        let (version, on_request) = ((3, 0), true);
        TurnMark {
            node: "n1",
            call_ref: "c",
            point,
            version,
            wrote: true,
            removed: false,
            on_request,
            called_out: false,
            awaits_answer: false,
            released: None,
        }
    }

    #[tokio::test]
    async fn no_probe_builds_no_mark() {
        TurnProbes::default().at(|| unreachable!("a mark built with no probe")).await;
    }

    #[tokio::test]
    async fn an_installed_probe_sees_every_sub_point_in_order() {
        let seen = Arc::new(Seen::default());
        let probes = TurnProbes::new(seen.clone());
        probes.at(|| mark(TurnPoint::Logged)).await;
        probes.at(|| mark(TurnPoint::Left)).await;
        assert_eq!(
            *seen.0.lock().unwrap(),
            vec![(TurnPoint::Logged, (3, 0)), (TurnPoint::Left, (3, 0))]
        );
    }
}
