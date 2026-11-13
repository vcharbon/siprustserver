//! The simulated lane: one cell's system under test stood in process
//! ([`SimulatedSut`]) on a shared simulated fabric under a paused clock.
//! Virtual time, so compressible dwells cost nothing. The ids the lane mints
//! derive from the case's seed (`crate::lanes::seed`), so two runs of one
//! document mint the same ones. The lane owns the runtime, the clock, the
//! actors on the agents the system bound, the run configuration and the
//! interpreter's run; the system owns its stand and the cell's end.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pivot_interpreter::Lane;
use pivot_schema::bundle::ClockMode;
use sip_clock::Clock;

use crate::lanes::{actors_on_endpoints, booking, panic_text, seed};
use crate::register::{SimulatedCell, SimulatedSut, Stood};
use crate::spec::{RunOverlay, SdpPlane};
use crate::{run_config, Case, Failure, RunSummary};

/// What the run-spec states of a simulated-lane cell.
pub struct SimulatedLane<'a> {
    /// Where the bundle is written.
    pub out_dir: &'a Path,
    pub overlay: &'a RunOverlay,
    /// The `endpoints[].id` whose bound socket the system's egress targets.
    pub egress_endpoint: &'a str,
    /// What the scripted peers do to the SDP they send.
    pub sdp: SdpPlane,
    /// The port book's base, read only under [`SdpPlane::Rewrite`].
    pub rtp_base: u16,
}

/// Runs `case` on the simulated lane against `sut`.
pub fn run(
    lane: &SimulatedLane<'_>,
    case: &Case,
    sut: &dyn SimulatedSut,
) -> Result<RunSummary, Failure> {
    let egress = case.egress_addr(lane.egress_endpoint)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .map_err(|e| Failure::Environment(format!("build runtime: {e}")))?;
    let seed = seed::seed_of(&case.document.case.id);
    let nonce = seed::cell_nonce(seed, 0);
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Failure::Environment(format!("read the wall clock: {e}")))?;
    let cell = SimulatedCell {
        case,
        out_dir: lane.out_dir,
        overlay: lane.overlay,
        egress,
        seed,
        nonce: &nonce,
        start,
    };
    let body = catch_unwind(AssertUnwindSafe(|| {
        let _trace_ids = observe::trace_ids::seed_thread(seed);
        let clock = rt.block_on(async { Clock::test_at(sut.anchor_ms(start)) });
        let Stood { ingress, agents: by_endpoint, identities, composer, mut run } =
            rt.block_on(sut.stand(&cell, clock.clone()))?;
        let agents = actors_on_endpoints(case, &by_endpoint)?;
        let config = run_config(
            sut.lane_name(),
            ClockMode::Virtual,
            ingress.to_string(),
            lane.overlay,
            identities,
        )
        .with_identity_nonce(nonce.clone());
        let (config, events) = run.directed(config);
        let lane_cfg = Lane {
            reader: run.reader(&agents)?,
            agents,
            route_target: ingress,
            media: booking(lane.sdp, "127.0.0.1", lane.rtp_base),
            base_dir: case.base_dir.clone(),
            composer: &*composer,
            events,
            injector: None,
        };
        let http_epoch_ms = rt.block_on(async { clock.now_ms() });
        let ran = catch_unwind(AssertUnwindSafe(|| {
            rt.block_on(pivot_interpreter::replay(
                case.document.clone(),
                config,
                lane_cfg,
                &*run,
                lane.out_dir,
            ))
        }));
        run.finish(&rt, ran, http_epoch_ms)
    }));
    body.map_err(|panic| Failure::Panicked(panic_text(panic.as_ref())))?
}
