//! The shape-**A** kill of the in-flight cells (`inflight_resend`) whose
//! callee answers inside the proxy's health lag, so the answer is lost with
//! the dead node and no owner of the call takes it.

use failover_harness::{FailoverHarness, ProxySut, ReplicatedB2buaSut, WorkerHealth};

use crate::inflight_resend::{crash_serving_node, HEALTH_LAG, KILL_SETTLE};

/// The shape-**A** kill whose callee answers inside the proxy's health lag:
/// the node dies holding the peer's request open, `answer` leaves the callee
/// while the proxy still reads the node alive, so the proxy forwards it onto
/// the dead node's socket, where it is lost, and only then stops routing
/// there. The peers are not told. Returns at T1 after the kill, with the
/// survivor's takeover count before it.
pub(crate) async fn kill_serving_node_answer_lost(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
    answer: impl std::future::Future<Output = ()>,
) -> u64 {
    let hydrated_before = crash_serving_node(fh, primary, survivor, pri_ord);
    answer.await;
    fh.advance(HEALTH_LAG).await;
    proxy.set_health(pri_ord, WorkerHealth::Dead);
    fh.advance(KILL_SETTLE).await;
    hydrated_before
}
