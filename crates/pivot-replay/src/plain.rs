//! The plain simulated system: what the simulated lane runs with nothing
//! registered. The B2BUA worker with no service, every new call
//! routed to the cell's egress endpoint (`ScriptedDecisionEngine::route_all_to`),
//! at [`INGRESS`] on the shared fabric. It runs a case whose calls the callee
//! answers or rejects as the document states (a basic call, a call the callee
//! refuses), and states no decision exchange, limiter or call record: a case
//! that needs a backend's decision, a service or a claim answered is a
//! product's, which registers its own system.
//!
//! Its post-run gate is the worker's reap check, judged at the interpreter's
//! settle and again once the harness settled the fabric: a breach fails the
//! verdict at the reap site and the cell's `cleanup` line ([`GATES`]). The
//! bundle holds the terminated calls and the worker's cap readings beside the
//! RFC audit.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::B2buaSut;
use pivot_interpreter::{LaneEvents, Sut};
use pivot_schema::bundle::RunConfig;
use scenario_harness::{Agent, Harness};
use sip_clock::Clock;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::compose::LaneComposer;
use crate::lanes::{panic_text, recorded, Ran, Record};
use crate::numbers::lane_numbers;
use crate::register::{SimulatedCell, SimulatedRun, SimulatedSut, Standing, Stood};
use crate::sut::WorkerSut;
use crate::verifications::{Gate, Gates, Standing::Judged};
use crate::{audit, cancellation, cap_readings, reap, terminated, verdict, verifications};
use crate::{Case, Failure, RunSummary};

/// The plain system's lane name.
pub const LANE_NAME: &str = "simulated";

/// The plain system's one gate: the worker's reap check.
pub const CLEANUP: &str = "cleanup";

/// The plain system's gate table: [`CLEANUP`], charged at the reap site.
pub static GATES: Gates = Gates {
    gates: &[Gate { name: CLEANUP, source: "lane gate", charges: reap::charges }],
    reference: None,
};

/// Where the plain worker takes SIP on the simulated fabric.
pub const INGRESS: &str = "192.0.2.1:5060";

/// The domain a From/To is composed at.
pub const DOMAIN: &str = "pivot.invalid";

/// How long an agent of the shared fabric waits for a datagram.
const RECV: Duration = Duration::from_secs(20);

/// The plain simulated system.
#[derive(Debug, Default)]
pub struct PlainB2bua {
    leaks_a_call: bool,
}

impl PlainB2bua {
    /// The plain system whose worker counts one call begun that it never
    /// removes: the reap check's breach with the wire exchange intact, a
    /// test seam.
    pub fn leaking_a_call() -> PlainB2bua {
        PlainB2bua { leaks_a_call: true }
    }
}

impl SimulatedSut for PlainB2bua {
    fn lane_name(&self) -> &str {
        LANE_NAME
    }

    fn anchor_ms(&self, _start: Duration) -> i64 {
        0
    }

    fn stand<'a>(&'a self, cell: &'a SimulatedCell<'a>, clock: Clock) -> Standing<'a> {
        Box::pin(stand(cell, clock, self.leaks_a_call))
    }
}

async fn stand<'a>(
    cell: &'a SimulatedCell<'a>,
    clock: Clock,
    leaks_a_call: bool,
) -> Result<Stood<'a>, Failure> {
    let ingress: SocketAddr = INGRESS.parse().expect("the plain ingress is a socket");
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let h = Harness::with_network_and_clock(
        format!("replay-{}", cell.case.document.case.id),
        sim as Arc<dyn sip_net::SignalingNetwork>,
        clock.clone(),
        layer_harness::TransportKind::Fake,
        RECV,
    );
    let decision =
        ScriptedDecisionEngine::route_all_to(cell.egress.ip().to_string(), cell.egress.port());
    let worker = B2buaSut::builder(Arc::new(decision))
        .clock(clock)
        .keep_terminated_calls()
        .start(&h, "b2bua", INGRESS)
        .await;
    if leaks_a_call {
        worker.metrics().bump_creation();
    }
    let mut agents = BTreeMap::new();
    for endpoint in &cell.case.document.endpoints {
        let addr = cell.case.endpoint_addrs[&endpoint.id];
        agents.insert(endpoint.id.clone(), h.agent(endpoint.id.as_str(), &addr.to_string()).await);
    }
    let (numbers, identities) = lane_numbers(&cell.case.plan, cell.overlay.identities.clone());
    let composer =
        LaneComposer { ingress, domain: DOMAIN.to_string(), numbers, trunk: String::new() };
    Ok(Stood {
        ingress,
        agents,
        identities,
        composer: Box::new(composer),
        run: Box::new(PlainRun { h, worker, case: cell.case, out_dir: cell.out_dir }),
    })
}

/// The plain worker running one cell, on the harness of the cell's fabric.
struct PlainRun<'a> {
    h: Harness,
    worker: B2buaSut,
    case: &'a Case,
    out_dir: &'a Path,
}

impl Sut for PlainRun<'_> {
    fn active_calls(&self) -> usize {
        WorkerSut(&self.worker).active_calls()
    }

    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        WorkerSut(&self.worker).observe(name)
    }
}

impl SimulatedRun for PlainRun<'_> {
    fn directed(&mut self, config: RunConfig) -> (RunConfig, Option<LaneEvents>) {
        (config, None)
    }

    fn reader(
        &self,
        _actors: &BTreeMap<String, Agent>,
    ) -> Result<Arc<dyn sip_message::SipParser + Send + Sync>, Failure> {
        Ok(Arc::new(sip_message::CustomParser::built()))
    }

    fn finish(
        self: Box<Self>,
        rt: &tokio::runtime::Runtime,
        ran: Ran,
        _http_epoch_ms: i64,
    ) -> Result<RunSummary, Failure> {
        let PlainRun { h, worker, case, out_dir } = *self;
        let calls = worker.terminated_calls();
        let records: Vec<Record<'_>> = vec![Box::new(|| terminated::record(out_dir, &calls))];
        let outcome = recorded(out_dir, ran, records)?;
        let at_settle = reaped(&worker);
        let (report, gating) = rt.block_on(h.finish_collecting());
        let settled = reaped(&worker);
        terminated::record(out_dir, &worker.terminated_calls())?;
        cap_readings::record(out_dir, worker.metrics())?;
        let stages = [at_settle, settled];
        let breaches: Vec<String> = stages.iter().filter_map(|r| r.clone().err()).collect();
        let cleanup = if breaches.is_empty() { Ok(()) } else { Err(breaches.join("; ")) };
        let mut verdict = outcome.verdict;
        reap::record(&stages, &mut verdict);
        verdict::write(out_dir, &verdict, &GATES)?;
        verifications::declare(out_dir, &verdict, &[(CLEANUP, Judged)], &GATES)?;
        let statements = cancellation::Statements::of(&case.document, outcome.recording.legs());
        let rfc = audit::audited(
            report.rfc_findings(),
            &gating,
            &sip_net_testkit::audit_wire_entries(report.events()),
            &audit::actors(&case.document.endpoints, &[&case.endpoint_addrs]),
            &statements,
        );
        audit::record(out_dir, &rfc)?;
        Ok(RunSummary {
            verdict: verdict.status,
            failures: verdict.failures.len(),
            cleanup,
            rfc,
            bundle: out_dir.to_path_buf(),
        })
    }
}

/// The worker's reap check as a result: its panic text on a breach.
fn reaped(worker: &B2buaSut) -> Result<(), String> {
    catch_unwind(AssertUnwindSafe(|| worker.assert_calls_reaped()))
        .map_err(|panic| panic_text(panic.as_ref()))
}
