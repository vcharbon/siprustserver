//! How a worker stops authoring: at a crash the harness simulates, and when
//! the membership binds its ordinal to another worker identity (ADR-0055 §6).
//! Either way nothing leaves the worker any more — no SIP, no store write a
//! peer can pull, no CDR — and no call is drained.

use std::sync::{Arc, Weak};

use tokio::task::AbortHandle;

use super::B2buaCore;
use crate::repl::ReplicationSupervisor;
use crate::router::RouterCtx;

/// What stopping the worker's authoring reaches.
pub(super) struct Fence {
    /// The worker's directly-spawned tasks: the router, the changelog server,
    /// the sweep, the samplers, the limiter's tasks and the withdrawal latch.
    pub(super) tasks: Vec<AbortHandle>,
    pub(super) ctx: Weak<RouterCtx>,
    pub(super) supervisor: Option<ReplicationSupervisor>,
}

impl Fence {
    /// Abort every task that turns a call, halt every in-flight turn, abort the
    /// transaction owner (it owns the SIP endpoint), stop the limiter's
    /// release queue and park every replication puller. Idempotent.
    pub(super) fn close(&self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some(ctx) = self.ctx.upgrade() {
            stop_ctx(&ctx);
        }
        if let Some(supervisor) = &self.supervisor {
            supervisor.shutdown();
        }
    }

    /// Close the fence the instant the worker observes itself superseded;
    /// never on a worker that is not.
    pub(super) async fn on_superseded(self: Arc<Self>) {
        let Some(supervisor) = &self.supervisor else {
            return std::future::pending().await;
        };
        let mut own = supervisor.self_endpoint();
        if own.wait_for(|own| own.is_superseded()).await.is_err() {
            return std::future::pending().await;
        }
        self.close();
    }
}

/// The context's part of a stop: in-flight turns halted, the transaction
/// owner aborted, the limiter's release queue stopped.
fn stop_ctx(ctx: &RouterCtx) {
    ctx.dispatcher.halt();
    // The transaction owner is spawned untracked inside
    // `TransactionLayer::spawn` and owns the SIP endpoint: without this the
    // worker keeps answering SIP until every per-call task drops its sender.
    ctx.txn.abort_owner();
    // A stop loses the release queue; a drain still waiting flushes nothing.
    ctx.limiter.stop();
}

impl B2buaCore {
    /// CRASH: abort the directly-spawned tasks (serve loop + router) and park
    /// every replication puller (closing its pulled connections). Mirrors the
    /// ha-harness `HaNode::crash` discipline at the live-core level: the spawned
    /// per-connection/per-puller tasks lose their driver and unwind, and dropping
    /// this `B2buaCore` afterwards releases the last store/supervisor `Arc`s so a
    /// reboot can re-listen on the same addresses. No per-call body runs on.
    pub fn abort(&mut self) {
        for t in std::mem::take(&mut self.tasks) {
            t.abort();
        }
        self.fence.close();
        // Per-call root spans are this core's runtime state, not replicated
        // state: they die with the core and return their active-trace slots
        // (ADR-0026). A survivor taking one of these calls over therefore opens
        // its OWN linked root, exactly as it does across a process boundary.
        let traces = crate::trace::traces();
        self.node.enter(|| {
            for call_ref in self.ctx.state.live_call_refs() {
                traces.close(&call_ref);
            }
        });
    }

    /// Whether this worker observed its ordinal bound to another worker
    /// identity and stopped authoring (ADR-0055 §6). Sticky. Always `false`
    /// without replication or on a membership that does not show the worker
    /// itself.
    pub fn is_superseded(&self) -> bool {
        self.supervisor.as_ref().is_some_and(|s| s.self_endpoint().borrow().is_superseded())
    }

    /// Resolves once this worker is superseded (ADR-0055 §6), its authoring
    /// stopped: the host exits without a drain. Never without replication or
    /// on a membership that does not show the worker itself.
    pub async fn superseded(&self) {
        if let Some(supervisor) = &self.supervisor {
            let mut own = supervisor.self_endpoint();
            if own.wait_for(|own| own.is_superseded()).await.is_ok() {
                self.fence.close();
                return;
            }
        }
        std::future::pending().await
    }
}
