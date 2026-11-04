//! The proxy's probe answers (ADR-0012 D4, ADR-0046 D4, ADR-0050 §11,
//! ADR-0056): `/readyz` is fit for traffic once the first worker view is in,
//! with at least one `Alive` worker whatever its acceptance of new calls,
//! every recv shard turning, the external face usable, and no drain latched.
//! The node announces the proxy's addresses through MetalLB while `/readyz`
//! passes. `/fitz` asks the same and, in place of the drain latch, that some
//! worker answered within the isolation window; the node's route guard
//! withdraws the node at once while it fails. `/healthz` is live while every
//! recv shard turns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use probe_http::{FitFn, Fitness, LiveFn, ProbeState, ReadyFn};
use sip_clock::Clock;
use sip_proxy::health::{FirstView, WorkerReach};
use sip_proxy::liveness::ShardPulse;
use sip_proxy::registry::WorkerRegistry;

use crate::external_face::FaceMonitor;

/// Why the proxy cannot carry traffic, as `/fitz` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unfit {
    ShardStalled,
    NoFirstView,
    ExternalFace,
    NoWorkerAlive,
    /// No worker answered a probe within the isolation window.
    Isolated,
}

impl Unfit {
    /// The token `/fitz` answers.
    pub fn reason(self) -> &'static str {
        match self {
            Unfit::ShardStalled => "shard-stalled",
            Unfit::NoFirstView => "no-first-view",
            Unfit::ExternalFace => "external-face",
            Unfit::NoWorkerAlive => "no-worker-alive",
            Unfit::Isolated => "isolated",
        }
    }
}

/// What `/readyz` asks beyond the drain latch, in the order it is asked.
fn serving(
    shard_stalled: bool,
    first_view: bool,
    external_face_usable: bool,
    registry: &dyn WorkerRegistry,
) -> Result<(), Unfit> {
    if shard_stalled {
        Err(Unfit::ShardStalled)
    } else if !first_view {
        Err(Unfit::NoFirstView)
    } else if !external_face_usable {
        Err(Unfit::ExternalFace)
    } else if !registry.snapshot().iter().any(|w| w.is_alive()) {
        Err(Unfit::NoWorkerAlive)
    } else {
        Ok(())
    }
}

/// Draining once SIGTERM latched; else NotReady while a recv shard is
/// stalled, before the first worker view, with the external face unusable,
/// or with no worker `Alive`; else Ready.
pub fn probe_state(
    draining: bool,
    shard_stalled: bool,
    first_view: bool,
    external_face_usable: bool,
    registry: &dyn WorkerRegistry,
) -> ProbeState {
    if draining {
        ProbeState::Draining
    } else if serving(shard_stalled, first_view, external_face_usable, registry).is_ok() {
        ProbeState::Ready
    } else {
        ProbeState::NotReady
    }
}

/// Unfit for the first cause `/readyz` would fail on, its drain latch
/// aside, else unfit while `isolated`; else fit.
pub fn fitness(
    shard_stalled: bool,
    first_view: bool,
    external_face_usable: bool,
    isolated: bool,
    registry: &dyn WorkerRegistry,
) -> Result<(), Unfit> {
    serving(shard_stalled, first_view, external_face_usable, registry)?;
    if isolated {
        Err(Unfit::Isolated)
    } else {
        Ok(())
    }
}

/// What the probe server answers with: `/readyz`, `/healthz` and `/fitz`.
pub struct ProxyProbes {
    pub ready: ReadyFn,
    pub live: LiveFn,
    pub fit: FitFn,
}

/// What the probe answers read.
pub struct ProbeSources {
    /// Latched by SIGTERM.
    pub draining: Arc<AtomicBool>,
    pub pulse: Arc<ShardPulse>,
    pub clock: Clock,
    /// A shard on one packet longer than this is stalled.
    pub stall_ms: u64,
    pub registry: Arc<dyn WorkerRegistry>,
    pub first_view: Arc<FirstView>,
    /// The external face's latest reading; `None`: no face check.
    pub external_face: Option<Arc<FaceMonitor>>,
    pub reach: Arc<WorkerReach>,
    /// No worker answer for longer reads the proxy isolated.
    pub isolation_window: Duration,
}

/// The probes over `sources`: a shard holding one packet longer than
/// `stall_ms` fails all three, so the node withdraws the addresses and the
/// livenessProbe restarts the process. A shard waiting on an empty socket is
/// never stalled.
pub fn probes(sources: ProbeSources) -> ProxyProbes {
    let ProbeSources {
        draining,
        pulse,
        clock,
        stall_ms,
        registry,
        first_view,
        external_face,
        reach,
        isolation_window,
    } = sources;
    let stalled = Arc::new(move || {
        !pulse.stalled(u64::try_from(clock.now_ms()).unwrap_or(0), stall_ms).is_empty()
    });
    let face_usable = Arc::new(move || external_face.as_ref().is_none_or(|face| face.usable()));
    let ready: ReadyFn = {
        let (stalled, face_usable, registry, first_view) =
            (stalled.clone(), face_usable.clone(), registry.clone(), first_view.clone());
        Arc::new(move || {
            probe_state(
                draining.load(Ordering::Relaxed),
                stalled(),
                first_view.is_in(),
                face_usable(),
                registry.as_ref(),
            )
        })
    };
    let fit: FitFn = {
        let stalled = stalled.clone();
        Arc::new(move || {
            match fitness(
                stalled(),
                first_view.is_in(),
                face_usable(),
                reach.isolated(isolation_window),
                registry.as_ref(),
            ) {
                Ok(()) => Fitness::Fit,
                Err(unfit) => Fitness::Unfit(unfit.reason()),
            }
        })
    };
    let live: LiveFn = Arc::new(move || !stalled());
    ProxyProbes { ready, live, fit }
}

#[cfg(test)]
mod tests;
