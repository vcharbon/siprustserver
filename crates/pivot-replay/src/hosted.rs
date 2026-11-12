//! The hosted lane: a SUT in its own process, on real UDP and a real
//! clock, one cell of the batch process that serves the SUT's backend double
//! (`crate::batch`). The lane owns what every hosted cell does alike: the
//! datagram preflight, the cell's own runtime, the endpoints bound on real
//! sockets with ids seeded from the batch seed and the cell's ordinal, the
//! URI composer and the identities, the run configuration, and the
//! interpreter's run under `catch_unwind`. The product's registered system
//! ([`HostedSystem`]) directs the cell (its decisions, its run configuration,
//! its lane events) and owns the cell's end ([`HostedRun::finish`]).
//!
//! Endpoints bind BEFORE the system is told anything: a run-spec's port `0`
//! is placed by the OS, and the address a decision hands the SUT as where the
//! b-leg is dialled has to be the one the endpoint actually holds.
//!
//! The settle oracle reads no `active_calls`, because the SUT is SHARED: the
//! gauge counts every cell in flight, so "is this call reaped" is a question
//! only the batch can ask.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pivot_interpreter::{Booking, DialogIdentity, Lane, LaneEvents};
use pivot_schema::bundle::{ClockMode, RecordedMessage, RunConfig, RunRfcAudit};
use scenario_harness::{Agent, AgentBinder};
use sip_clock::Clock;

use crate::compose::LaneComposer;
use crate::lanes::bind::Bound;
use crate::lanes::preflight::carries_full_datagrams;
use crate::lanes::{actors_on_endpoints, booking, panic_text, seed, Ran};
use crate::numbers::lane_numbers;
use crate::spec::{RunOverlay, SdpPlane};
use crate::sut::HostedSut;
use crate::{audit, cancellation, run_config, sut_join, Case, Failure, RunSummary};

/// How long a hosted cell's endpoint transactions wait on the wire.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(40);

/// Unix milliseconds, the clock a backend double's record stamps its rows with.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// What the run-spec states of a hosted-lane cell.
pub struct HostedLane<'a> {
    /// The lane name the bundle states.
    pub name: &'a str,
    /// Where the bundle is written.
    pub out_dir: &'a Path,
    pub overlay: &'a RunOverlay,
    /// Where the SUT takes SIP.
    pub ingress: SocketAddr,
    /// Where the SUT publishes Prometheus text, where it publishes any: a
    /// document's observables are read there.
    pub metrics: Option<SocketAddr>,
    /// The `endpoints[].id` whose bound socket the system's egress targets.
    pub egress_endpoint: &'a str,
    /// What the scripted peers do to the SDP they send.
    pub sdp: SdpPlane,
    /// The port book's base, read only under [`SdpPlane::Rewrite`].
    pub rtp_base: u16,
    /// The domain and trunk a tier-2 position composes into a URI with.
    pub domain: &'a str,
    pub trunk: &'a str,
}

impl HostedLane<'_> {
    /// The media booking the cell's peers send under: none under
    /// [`SdpPlane::Verbatim`], the `egress` address and the port book from
    /// `rtp_base` under [`SdpPlane::Rewrite`].
    pub fn media(&self, egress: SocketAddr) -> Booking {
        booking(self.sdp, &egress.ip().to_string(), self.rtp_base)
    }
}

/// One cell of a hosted lane, as the lane hands it to the registered system.
pub struct HostedCell<'a> {
    pub case: &'a Case,
    pub lane: &'a HostedLane<'a>,
    /// Where the system dials the cell's b-leg (`Bound::egress_addr`).
    pub egress: SocketAddr,
    /// Every endpoint's bound socket, by endpoint id.
    pub addrs: &'a BTreeMap<String, SocketAddr>,
    /// The identity nonce the cell's dialog identities are minted under.
    pub nonce: &'a str,
    /// The dialog identity each call of the document is placed under, in
    /// document order (`crate::sut_join::minted`).
    pub seeds: &'a [DialogIdentity],
    /// How a tier-2 position becomes a URI on this lane.
    pub composer: &'a LaneComposer,
}

/// A product's system behind a hosted lane: directs each cell before it runs.
pub trait HostedSystem {
    /// The cell's run, once the system was told what the cell places: what
    /// it decides for the cell is served from here on.
    fn direct<'a>(&'a self, cell: &HostedCell<'a>) -> Result<Box<dyn HostedRun + 'a>, Failure>;
}

/// The run of one hosted-lane cell: what the lane leaves to the system before
/// and after the interpreter's run.
pub trait HostedRun {
    /// The run configuration the cell runs under, from the lane's, and the
    /// lane events the system reports, where it reports any.
    fn directed(&mut self, config: RunConfig) -> (RunConfig, Option<LaneEvents>);

    /// How the run reads its parties' recording, stated to every agent in
    /// `actors` as well.
    fn reader(
        &self,
        actors: &BTreeMap<String, Agent>,
    ) -> Result<Arc<dyn sip_message::SipParser + Send + Sync>, Failure>;

    /// The cell's end, once the interpreter ran (`ran`): the bundle's records,
    /// the RFC audit over `wire`, the system's gates and verifications.
    /// `http_epoch_ms` is the Unix clock reading the run's recording counts
    /// from.
    fn finish(
        self: Box<Self>,
        ran: Ran,
        http_epoch_ms: i64,
        wire: Wire<'_>,
    ) -> Result<RunSummary, Failure>;
}

/// The wire the cell's endpoints recorded, for its RFC audit.
pub struct Wire<'a> {
    binder: &'a AgentBinder,
    case: &'a Case,
    addrs: &'a BTreeMap<String, SocketAddr>,
}

impl Wire<'_> {
    /// The suite over the endpoints' recorded wire, `legs` the run's
    /// recording by leg; the gating subset is what the load driver gates on
    /// (no waivers: a replay declares none).
    pub fn audit(&self, legs: BTreeMap<String, Vec<RecordedMessage>>) -> RunRfcAudit {
        let statements = cancellation::Statements::of(&self.case.document, legs);
        match self.binder.rfc_audit() {
            Some((all, entries)) => audit::audited(
                &all,
                &self.binder.rfc_findings(&[]).findings,
                &entries,
                &audit::actors(
                    &self.case.document.endpoints,
                    &[&self.case.endpoint_addrs, self.addrs],
                ),
                &statements,
            ),
            None => RunRfcAudit::NotAudited { reason: "the lane's binder recorded no wire".into() },
        }
    }
}

/// Runs `case` as the `ordinal`-th cell of the batch seeded `batch_seed` on
/// `lane`, against `system`, under `catch_unwind`: a panic anywhere in it is
/// `Failure::Panicked`, the batch's other cells untouched.
pub fn run(
    lane: &HostedLane<'_>,
    case: &Case,
    batch_seed: u64,
    ordinal: u64,
    system: &dyn HostedSystem,
) -> Result<RunSummary, Failure> {
    catch_unwind(AssertUnwindSafe(|| body(lane, case, batch_seed, ordinal, system)))
        .map_err(|panic| Failure::Panicked(panic_text(panic.as_ref())))?
}

fn body(
    lane: &HostedLane<'_>,
    case: &Case,
    batch_seed: u64,
    ordinal: u64,
    system: &dyn HostedSystem,
) -> Result<RunSummary, Failure> {
    carries_full_datagrams(case, &[lane.ingress.ip()])?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::Environment(format!("build runtime: {e}")))?;
    let binder = AgentBinder::real(Clock::system(), ENDPOINT_TIMEOUT, true);
    binder.seed_ids((batch_seed ^ ordinal).max(1));
    let nonce = seed::cell_nonce(batch_seed, ordinal);
    let bound: Bound = rt.block_on(crate::lanes::bind::endpoints(&binder, case));
    let egress = bound.egress_addr(lane.egress_endpoint, case.no_dial())?;
    let agents = actors_on_endpoints(case, &bound.agents)?;
    let (numbers, identities) = lane_numbers(&case.plan, lane.overlay.identities.clone());
    let composer = LaneComposer {
        ingress: lane.ingress,
        domain: lane.domain.to_string(),
        numbers,
        trunk: lane.trunk.to_string(),
    };
    let seeds = sut_join::minted(&case.document, &nonce);
    let cell = HostedCell {
        case,
        lane,
        egress,
        addrs: &bound.addrs,
        nonce: &nonce,
        seeds: &seeds,
        composer: &composer,
    };
    let mut run = system.direct(&cell)?;
    let config =
        run_config(lane.name, ClockMode::Real, lane.ingress.to_string(), lane.overlay, identities)
            .with_identity_nonce(nonce.clone());
    let (config, events) = run.directed(config);
    let lane_cfg = Lane {
        reader: run.reader(&agents)?,
        agents,
        route_target: lane.ingress,
        media: lane.media(egress),
        base_dir: case.base_dir.clone(),
        composer: &composer,
        events,
    };
    let sut = HostedSut { metrics: lane.metrics };
    let http_epoch_ms = now_ms();
    let ran = catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(pivot_interpreter::replay(
            case.document.clone(),
            config,
            lane_cfg,
            &sut,
            lane.out_dir,
        ))
    }));
    let wire = Wire { binder: &binder, case, addrs: &bound.addrs };
    run.finish(ran, http_epoch_ms, wire)
}
