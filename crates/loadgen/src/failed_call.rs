//! What a call owes the SUT before its endpoints drop. A failed call: its
//! teardown transactions run to their end, and its callee legs answer what
//! the SUT still sends them; then what the call leaves once its traffic is
//! over. A call that did not fail, with a client transaction still open at
//! its end, is held the same way with no quiesce; a recorded one settles in
//! place instead: that transaction runs to its end before the call is
//! audited and recorded.

use std::sync::Arc;
use std::time::Duration;

use scenario_harness::Agent;
use tokio::sync::{oneshot, OwnedRwLockReadGuard, OwnedSemaphorePermit, RwLock};

use crate::mux::MuxNetwork;

/// What runs once a call's traffic is over.
type After = Box<dyn FnOnce() + Send>;

/// Where a call hands what runs once its traffic is over: at once for a call
/// not held, after its release for a held one.
pub(crate) struct Tail(Option<oneshot::Sender<After>>);

impl Tail {
    /// Fold the call's messages into its ladder once its traffic is over;
    /// write its record then too when the call awaited a final, else now.
    pub(crate) fn finish(self, fold: After, write: After, awaited: bool) {
        let after: After = match awaited {
            true => Box::new(move || {
                fold();
                write();
            }),
            false => {
                write();
                fold
            }
        };
        match self.0 {
            Some(tx) => drop(tx.send(after)),
            None => after(),
        }
    }
}

/// What keeps the driver's drain waiting on a call until the call's task
/// ends: its in-flight permit, or, for a call settled in place, `tails`
/// read-locked, its permit freed while it settles.
pub(crate) enum Held {
    Permit { _permit: OwnedSemaphorePermit },
    Tails { _tails: OwnedRwLockReadGuard<()> },
}

/// How a call stands at its verdict.
pub(crate) struct CallEnd {
    /// The call's scenario failed.
    pub(crate) failed: bool,
    /// A client transaction of its endpoints is still open (RFC 3261 §17.1.2).
    pub(crate) open: bool,
    /// The call records, so its RFC audit reads the recording.
    pub(crate) recorded: bool,
}

/// Hold the call when it failed, or did not fail with a client transaction
/// still open, releasing it in the background ([`release`]); a recorded call
/// of the latter settles in place instead: every client transaction of its
/// endpoints runs to its end, retransmitted until its final, so the audit
/// that follows reads the final a retransmission draws after the verdict.
/// Any other call ends at once.
pub(crate) async fn hold(
    end: CallEnd,
    permit: OwnedSemaphorePermit,
    tails: Arc<RwLock<()>>,
    net: Arc<MuxNetwork>,
    (caller, callees): (&Agent, &[(&str, Agent)]),
    quiesce: Duration,
) -> (Tail, Held) {
    let CallEnd { failed, open, recorded } = end;
    let quiesce = match (failed, open, recorded) {
        (false, false, _) => return (Tail(None), Held::Permit { _permit: permit }),
        (false, true, true) => {
            let held = tails.read_owned().await;
            drop(permit);
            net.client_txns_ended().await;
            return (Tail(None), Held::Tails { _tails: held });
        }
        (true, ..) => quiesce,
        (false, true, false) => Duration::ZERO,
    };
    let tail = release(tails, net, caller, callees, quiesce).await;
    (tail, Held::Permit { _permit: permit })
}

/// Release a held call in the background, holding its endpoints until done
/// and `tails` read-locked meanwhile (the driver's drain write-locks it):
/// every client transaction open on its endpoints runs to its end, its
/// teardown CANCEL or BYE retransmitted until its final (RFC 3261 §17.1.2),
/// while for `quiesce` its callee legs answer what the SUT still sends them;
/// then it runs what the returned [`Tail`] is handed.
async fn release(
    tails: Arc<RwLock<()>>,
    net: Arc<MuxNetwork>,
    caller: &Agent,
    callees: &[(&str, Agent)],
    quiesce: Duration,
) -> Tail {
    let (tail, then) = oneshot::channel::<After>();
    let held = tails.read_owned().await;
    let caller = caller.clone();
    let callees: Vec<Agent> = callees.iter().map(|(_, agent)| agent.clone()).collect();
    tokio::spawn(async move {
        let answer = async {
            if !quiesce.is_zero() {
                for agent in &callees {
                    agent.release_failed_call(quiesce).await;
                }
            }
        };
        tokio::join!(net.client_txns_ended(), answer);
        if let Ok(then) = then.await {
            then();
        }
        drop((held, caller, callees));
    });
    Tail(Some(tail))
}
