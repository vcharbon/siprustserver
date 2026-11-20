//! The registration seam (ADR-0060 appendix §13): what a product
//! registers with the replay, and when the replay calls it.
//!
//! | A product registers | Called |
//! |---|---|
//! | its simulated system under test, [`SimulatedSut`] | per cell on the simulated lane ([`crate::simulated::run`]) |
//! | its hosted-lane system, [`crate::hosted::HostedSystem`] | per cell on a hosted lane ([`crate::hosted::run`]) |
//! | its backend double, [`crate::batch::BackendDouble`], with its batch parts, [`crate::batch::Registered`] | per batch, to serve or relay the system's decision requests |
//! | in-process verifications: its gate table, [`crate::verifications::Gates`], the tail's kept results, [`crate::tail::Kept`], the batch's end gates, [`crate::batch::Registered::end`] | at a cell's end and at a batch's drain; each answers lines |
//! | exports and keys ([`crate::exports`]) | at a cell's end and at a batch's drain |
//!
//! With nothing registered the simulated lane runs the plain system
//! ([`crate::plain`]), whose gate table holds its one `cleanup` gate, the
//! worker's reap check; every other verdict failure is the `call` line's.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pivot_interpreter::{LaneEvents, Sut, UriComposer};
use pivot_schema::bundle::{IdentityBindings, RunConfig};
use scenario_harness::Agent;
use sip_clock::Clock;

use crate::lanes::Ran;
use crate::spec::RunOverlay;
use crate::{Case, Failure, RunSummary};

/// One cell of the simulated lane, as the replay hands it to the registered
/// system: what the run-spec states and what the lane derived for it.
pub struct SimulatedCell<'a> {
    pub case: &'a Case,
    /// The cell's bundle directory.
    pub out_dir: &'a Path,
    pub overlay: &'a RunOverlay,
    /// Where the system dials the cell's b-leg (`Case::egress_addr`).
    pub egress: SocketAddr,
    /// What every id the lane mints derives from (`crate::lanes::seed`).
    pub seed: u64,
    /// The identity nonce the cell's dialog identities are minted under.
    pub nonce: &'a str,
    /// The wall clock at the cell's start, since the Unix epoch.
    pub start: Duration,
}

/// A future the simulated lane drives on the cell's paused runtime.
pub type Standing<'a> = Pin<Box<dyn Future<Output = Result<Stood<'a>, Failure>> + 'a>>;

/// A simulated system under test: stood up per cell on a shared simulated
/// fabric under a paused clock, the cell's parties bound beside it.
pub trait SimulatedSut {
    /// The lane name the bundle states (`RunConfig::lane`), which a
    /// document's `origin_lane` is compared against.
    fn lane_name(&self) -> &str;

    /// Where the cell's paused clock is anchored, in epoch ms, for a cell that
    /// started at `start`.
    fn anchor_ms(&self, start: Duration) -> i64;

    /// Stands the system and binds one agent per endpoint of the cell's
    /// document, on the runtime the lane drives and on `clock`.
    fn stand<'a>(&'a self, cell: &'a SimulatedCell<'a>, clock: Clock) -> Standing<'a>;
}

/// A system stood for one cell, with the cell's parties.
pub struct Stood<'a> {
    /// Where the system takes SIP.
    pub ingress: SocketAddr,
    /// Endpoint id → the agent playing it.
    pub agents: BTreeMap<String, Agent>,
    /// The identity bindings the run states.
    pub identities: IdentityBindings,
    /// How a tier-2 position becomes a URI on this lane.
    pub composer: Box<dyn UriComposer + 'a>,
    /// The running system.
    pub run: Box<dyn SimulatedRun + 'a>,
}

/// The running system of one cell: the interpreter's settle oracle, and what
/// the lane leaves to it before and after the run.
pub trait SimulatedRun: Sut {
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
    /// the system's post-run gates, its verifications and exports, and the RFC
    /// audit. `http_epoch_ms` is the clock reading the run's recording counts
    /// from; `rt` the cell's paused runtime.
    fn finish(
        self: Box<Self>,
        rt: &tokio::runtime::Runtime,
        ran: Ran,
        http_epoch_ms: i64,
    ) -> Result<RunSummary, Failure>;
}
