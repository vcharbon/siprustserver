//! `pivot-replay batch <batch-spec.json>` with nothing registered: the batch
//! process of a hosted lane whose system is the plain hosted one
//! (`crate::plain_hosted`). It serves no backend double: the system decides
//! each route itself, off the dial's `X-Api-Call`. Each cell's run-spec is the
//! binary's own, its lane block `hosted`.
//!
//! Its end answers the plain system's `cleanup` gate for every cell of the
//! batch: the system's `b2bua_active_calls` at the batch-spec's `metrics`
//! reads 0 within [`REAP_WITHIN`]; a gauge left above it fails every cell's
//! `cleanup` line, one that cannot be read leaves them not checkable.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pivot_interpreter::DialogIdentity;
use serde::Deserialize;

use crate::batch::gates::Gate;
use crate::batch::{BackendDouble, Ended, Host, Placed, Registered};
use crate::hosted::{self, HostedLane};
use crate::plain::{CLEANUP, DOMAIN, GATES};
use crate::plain_hosted::PlainHosted;
use crate::spec::{LaneSpec, RunSpec};
use crate::verifications::{Gates, Standing};
use crate::{reap, verdict, Case, Failure, RunSummary};

/// How long the batch's end waits for the system to hold no call.
pub const REAP_WITHIN: Duration = Duration::from_secs(10);

/// How often the end reads the gauge again while it waits.
const POLL: Duration = Duration::from_millis(250);

/// The batch-spec the harness writes for the plain hosted lane.
#[derive(Debug, Deserialize)]
pub struct BatchSpec {
    /// The lane every cell names.
    pub lane: String,
    /// Where the system publishes Prometheus text, `ip:port`; its absence
    /// leaves the cleanup gate not checkable.
    #[serde(default)]
    pub metrics: Option<String>,
}

/// The backend double of a system that asks none: it serves nothing, and its
/// address is the unspecified one.
pub struct Unserved;

impl BackendDouble for Unserved {
    type Row = ();

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], 0))
    }

    fn rows(&self) -> Vec<()> {
        Vec::new()
    }

    fn rows_of(&self, _keys: &BTreeSet<String>) -> Vec<()> {
        Vec::new()
    }

    fn key_of(&self, call: &DialogIdentity) -> String {
        call.call_id.clone()
    }

    fn process_us(_row: &()) -> Option<u64> {
        None
    }
}

/// The plain hosted lane's batch.
pub struct PlainBatch {
    metrics: Option<SocketAddr>,
}

impl Registered for PlainBatch {
    type Spec = BatchSpec;
    type Backend = Unserved;
    type Report = serde_json::Value;

    fn lane(spec: &BatchSpec) -> &str {
        &spec.lane
    }

    fn gates() -> &'static Gates {
        &GATES
    }

    async fn stand(spec: &BatchSpec, _seed: u64) -> Result<(PlainBatch, Unserved), Failure> {
        let metrics =
            spec.metrics.as_deref().map(|stated| socket_of("metrics", stated)).transpose()?;
        Ok((PlainBatch { metrics }, Unserved))
    }

    fn cell(host: &Host<PlainBatch>, path: &Path, ordinal: u64) -> Result<RunSummary, Failure> {
        let spec = RunSpec::read(path)?;
        let LaneSpec::Hosted(block) = &spec.lane else {
            return Err(Failure::Refused(format!(
                "{}: a batch runs hosted cells only; this one names the simulated lane",
                path.display()
            )));
        };
        if block.name != host.lane {
            return Err(Failure::Refused(format!(
                "{}: the cell names lane {}, the batch runs {}",
                path.display(),
                block.name,
                host.lane
            )));
        }
        let case = Case::load(&spec.case, false, &spec.run.endpoint_addresses)?;
        let lane = HostedLane {
            name: &block.name,
            out_dir: &spec.out_dir,
            overlay: &spec.run,
            ingress: socket_of("ingress", &block.ingress)?,
            metrics: host.metrics,
            egress_endpoint: &block.egress_endpoint,
            sdp: block.sdp,
            rtp_base: block.rtp_base,
            domain: DOMAIN,
            trunk: "",
        };
        let system = PlainHosted { host, out_dir: &spec.out_dir };
        hosted::run(&lane, &case, host.seed, ordinal, &system)
    }

    async fn end(host: &Host<PlainBatch>, placed: &[Placed]) -> Ended<serde_json::Value> {
        let (gate, standing) = match host.metrics {
            None => {
                let why = "the lane states no metrics address to read the system's calls at";
                (Gate::unchecked(CLEANUP, why.into()), Standing::Unchecked(why.into()))
            }
            Some(metrics) => {
                host.waiting.step(format!(
                    "the system to hold no call (at most {} s)",
                    REAP_WITHIN.as_secs()
                ));
                match reaped(metrics, REAP_WITHIN).await {
                    Reaped::Clean => (Gate::over(CLEANUP, None), Standing::Judged),
                    Reaped::Held(why) => (Gate::over(CLEANUP, Some(why)), Standing::Judged),
                    Reaped::Unread(why) => {
                        (Gate::unchecked(CLEANUP, why.clone()), Standing::Unchecked(why))
                    }
                }
            }
        };
        let charged = match &gate.message {
            Some(why) if gate.answer == crate::verifications::Answer::Fail => charge(placed, why),
            _ => Vec::new(),
        };
        Ended {
            gates: vec![gate],
            charged,
            settled: vec![(CLEANUP, standing)],
            report: serde_json::json!({}),
        }
    }
}

/// `stated` as a socket, or the batch-spec refused naming its `field`.
fn socket_of(field: &str, stated: &str) -> Result<SocketAddr, Failure> {
    stated.parse().map_err(|e| Failure::Refused(format!("{field} {stated:?}: {e}")))
}

/// Every placed cell's verdict failed with the reap breach `why`; the
/// bundles charged.
fn charge(placed: &[Placed], why: &str) -> Vec<PathBuf> {
    placed
        .iter()
        .filter_map(|cell| {
            let mut charged = verdict::read(&cell.bundle).ok()?;
            reap::record(&[Err(why.to_string())], &mut charged);
            verdict::write(&cell.bundle, &charged, &GATES).ok()?;
            Some(cell.bundle.clone())
        })
        .collect()
}

/// What the system's gauge read at the batch's end.
#[derive(Debug, PartialEq, Eq)]
pub enum Reaped {
    /// It read 0.
    Clean,
    /// It never read 0 within the bound: what it last read.
    Held(String),
    /// It was never read.
    Unread(String),
}

/// Reads `b2bua_active_calls` at `metrics` until it is 0 or `within` elapsed.
pub async fn reaped(metrics: SocketAddr, within: Duration) -> Reaped {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = None;
    loop {
        let read = tokio::task::spawn_blocking(move || crate::sut::active_calls(metrics))
            .await
            .ok()
            .flatten();
        match read {
            Some(0) => return Reaped::Clean,
            Some(held) => last = Some(held),
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return match last {
                Some(held) => Reaped::Held(format!(
                    "the system still held {held} call(s) {} ms after the batch's last cell: \
                     b2bua_active_calls at {metrics}",
                    within.as_millis()
                )),
                None => Reaped::Unread(format!("b2bua_active_calls at {metrics} never answered")),
            };
        }
        tokio::time::sleep(POLL).await;
    }
}
