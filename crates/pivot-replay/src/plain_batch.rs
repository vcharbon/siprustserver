//! `pivot-replay batch <batch-spec.json>` with nothing registered: the batch
//! process of a hosted lane whose system is the plain hosted one
//! (`crate::plain_hosted`). It serves no backend double: the system decides
//! each route itself, off the dial's `X-Api-Call`. Each cell's run-spec is the
//! binary's own, its lane block `hosted`.
//!
//! Its end answers the plain system's `cleanup` gate for every cell of the
//! batch: each of [`HELD`] at the batch-spec's `metrics` reads 0 within its
//! `metrics_reap_ms`; one left above 0 fails every cell's `cleanup` line, a
//! set that cannot be read leaves them not checkable.

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

/// How long the batch's end waits for the system to hold no call, where the
/// batch-spec states no `metrics_reap_ms`.
pub const REAP_WITHIN_MS: u64 = 10_000;

/// The longest `metrics_reap_ms` a batch-spec may state: ten minutes.
pub const REAP_CEILING_MS: u64 = 600_000;

/// The gauges that each read 0 on a system holding no call: its active
/// calls, its call map's entries and its SIP routing index's keys. A call
/// the active gauge lets go while the store keeps it is a store-side leak.
pub const HELD: [&str; 3] = ["b2bua_active_calls", "b2bua_store_calls", "b2bua_store_sip_index"];

/// How often the end reads the gauges again while it waits.
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
    /// How long the batch's end waits for [`HELD`] to read 0, above 0 and at
    /// most [`REAP_CEILING_MS`]. The gate trusts each scrape to read the
    /// system as it is (the upstream runner samples its store at the scrape).
    #[serde(default = "reap_within_ms", deserialize_with = "reap_ms_of")]
    pub metrics_reap_ms: u64,
}

/// The `metrics_reap_ms` a batch-spec stating none waits.
fn reap_within_ms() -> u64 {
    REAP_WITHIN_MS
}

/// A stated `metrics_reap_ms`, refused at 0 or past [`REAP_CEILING_MS`].
fn reap_ms_of<'de, D: serde::Deserializer<'de>>(stated: D) -> Result<u64, D::Error> {
    let ms = u64::deserialize(stated)?;
    if ms == 0 || ms > REAP_CEILING_MS {
        return Err(serde::de::Error::custom(format!(
            "metrics_reap_ms {ms}: a wait above 0 and at most {REAP_CEILING_MS} ms"
        )));
    }
    Ok(ms)
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
    reap_within: Duration,
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
        let reap_within = Duration::from_millis(spec.metrics_reap_ms);
        Ok((PlainBatch { metrics, reap_within }, Unserved))
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
                    "the system to hold no call (at most {} ms)",
                    host.reap_within.as_millis()
                ));
                match reaped(metrics, host.reap_within).await {
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

/// What the system's gauges read at the batch's end.
#[derive(Debug, PartialEq, Eq)]
pub enum Reaped {
    /// Each of [`HELD`] read 0 on one scrape.
    Clean,
    /// They never all read 0 within the bound: those the last scrape read
    /// above 0.
    Held(String),
    /// They were never read.
    Unread(String),
}

/// Reads [`HELD`] at `metrics` until each is 0 on one scrape or `within`
/// elapsed.
pub async fn reaped(metrics: SocketAddr, within: Duration) -> Reaped {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = None;
    loop {
        let read = tokio::task::spawn_blocking(move || crate::sut::gauges(metrics, &HELD))
            .await
            .ok()
            .flatten();
        match read {
            Some(read) if read.iter().all(|&n| n == 0) => return Reaped::Clean,
            Some(read) => last = Some(read),
            None => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return match last {
                Some(read) => Reaped::Held(format!(
                    "the system still held {} {} ms after the batch's last cell, at {metrics}",
                    above_zero(&read),
                    within.as_millis()
                )),
                None => Reaped::Unread(format!("{} at {metrics} never answered", HELD.join(", "))),
            };
        }
        tokio::time::sleep(POLL.min(deadline - now)).await;
    }
}

/// The gauges of [`HELD`] that `read` states above 0, each with its value.
fn above_zero(read: &[u64]) -> String {
    HELD.iter()
        .zip(read)
        .filter(|(_, &n)| n > 0)
        .map(|(name, n)| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}
