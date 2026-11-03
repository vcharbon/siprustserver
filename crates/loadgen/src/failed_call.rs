//! What a failed call owes the SUT before its endpoints drop: its teardown
//! transactions run to their end, and its callee legs answer what the SUT
//! still sends them; then what the call leaves once its traffic is over. A
//! call whose in-dialog request awaits its final at its end is held the same
//! way, with no quiesce.

use std::sync::Arc;
use std::time::Duration;

use scenario_harness::Agent;
use tokio::sync::{oneshot, RwLock};

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

/// Hold the call when it `failed` or `awaits` the final of an in-dialog
/// request, releasing it in the background ([`release`]); a call neither
/// failed nor awaiting a final ends at once.
pub(crate) async fn hold(
    failed: bool,
    awaits: bool,
    tails: Arc<RwLock<()>>,
    net: Arc<MuxNetwork>,
    caller: &Agent,
    callees: &[(&str, Agent)],
    quiesce: Duration,
) -> Tail {
    match (failed, awaits) {
        (false, false) => Tail(None),
        (true, _) => release(tails, net, caller, callees, quiesce).await,
        (false, true) => release(tails, net, caller, callees, Duration::ZERO).await,
    }
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
