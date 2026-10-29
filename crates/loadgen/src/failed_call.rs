//! What a failed call owes the SUT before its endpoints drop: its teardown
//! transactions run to their end, and its callee legs answer what the SUT
//! still sends them; then what the call leaves once its traffic is over.

use std::sync::Arc;
use std::time::Duration;

use scenario_harness::Agent;
use tokio::sync::{oneshot, RwLock};

use crate::mux::MuxNetwork;

/// Release a failed call in the background, holding its endpoints until
/// done and `tails` read-locked meanwhile (the driver's drain write-locks
/// it): every client transaction open on its endpoints runs to its end, its
/// teardown CANCEL or BYE retransmitted until its final (RFC 3261 §17.1.2),
/// while for `quiesce` its callee legs answer what the SUT still sends them;
/// then it runs what `then` hands over, if the call sends anything.
pub(crate) async fn release(
    tails: Arc<RwLock<()>>,
    net: Arc<MuxNetwork>,
    caller: &Agent,
    callees: &[(&str, Agent)],
    quiesce: Duration,
    then: oneshot::Receiver<Box<dyn FnOnce() + Send>>,
) {
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
}
