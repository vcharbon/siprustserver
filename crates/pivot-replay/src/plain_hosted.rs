//! The plain hosted system: what a hosted lane runs with nothing registered.
//! A B2BUA in its own process whose decisions honour a dial's `X-Api-Call`
//! destination (the upstream B2BUA runner's `route_all_to_with_limiter`), so
//! each call of a cell is pinned to the cell's egress endpoint by that header
//! on the INVITE opening its caller leg ([`destination_of`]). No backend is
//! asked anything, and nothing is exported: the system's records stay in its
//! process, for a collector to read.
//!
//! Its one gate is the plain system's `cleanup`, answered by the batch's end
//! (`crate::plain_batch`): the B2BUA is shared by every cell of the batch, so
//! only the batch reads its gauge as a fact.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use pivot_interpreter::LaneEvents;
use pivot_schema::bundle::RunConfig;
use scenario_harness::Agent;

use crate::batch::Host;
use crate::hosted::{HostedCell, HostedRun, HostedSystem, Wire};
use crate::lanes::{recorded, Ran};
use crate::plain::{CLEANUP, GATES};
use crate::plain_batch::PlainBatch;
use crate::verifications::Standing;
use crate::{audit, verdict, verifications, Case, Failure, RunSummary};

/// The header a dial carries its b-leg's destination in.
pub const DESTINATION_HEADER: &str = "X-Api-Call";

/// The `X-Api-Call` value pinning a call's b-leg to `egress`.
pub fn destination_of(egress: SocketAddr) -> String {
    serde_json::json!({ "destination": { "host": egress.ip().to_string(), "port": egress.port() } })
        .to_string()
}

/// The plain system behind a hosted lane, for the cells of one batch.
pub struct PlainHosted<'a> {
    pub host: &'a Host<PlainBatch>,
    /// Where the cell's bundle is written.
    pub out_dir: &'a std::path::Path,
}

impl HostedSystem for PlainHosted<'_> {
    fn direct<'a>(&'a self, cell: &HostedCell<'a>) -> Result<Box<dyn HostedRun + 'a>, Failure> {
        self.host.minted(self.out_dir, cell.seeds);
        Ok(Box::new(PlainHostedRun {
            host: self.host,
            case: cell.case,
            out_dir: self.out_dir,
            egress: cell.egress,
        }))
    }
}

/// One cell of the plain hosted system.
struct PlainHostedRun<'a> {
    host: &'a Host<PlainBatch>,
    case: &'a Case,
    out_dir: &'a std::path::Path,
    egress: SocketAddr,
}

impl HostedRun for PlainHostedRun<'_> {
    /// Every call of the document dials with its b-leg pinned to the cell's
    /// egress.
    fn directed(&mut self, config: RunConfig) -> (RunConfig, Option<LaneEvents>) {
        let destination = destination_of(self.egress);
        let config = self.case.document.calls.iter().fold(config, |config, call| {
            config.with_call_header(&call.id, DESTINATION_HEADER, destination.clone())
        });
        (config, None)
    }

    fn reader(
        &self,
        _actors: &BTreeMap<String, Agent>,
    ) -> Result<Arc<dyn sip_message::SipParser + Send + Sync>, Failure> {
        Ok(Arc::new(sip_message::CustomParser::built()))
    }

    /// The verdict and the RFC audit over the endpoints' wire; `cleanup`
    /// pending until the batch's end answers it.
    fn finish(
        self: Box<Self>,
        ran: Ran,
        http_epoch_ms: i64,
        wire: Wire<'_>,
    ) -> Result<RunSummary, Failure> {
        let PlainHostedRun { host, out_dir, .. } = *self;
        let outcome = recorded(out_dir, ran, Vec::new())?;
        host.finished(out_dir, http_epoch_ms);
        let rfc = wire.audit(outcome.recording.legs());
        audit::record(out_dir, &rfc)?;
        let verdict = outcome.verdict;
        verdict::write(out_dir, &verdict, &GATES)?;
        verifications::declare(out_dir, &verdict, &[(CLEANUP, Standing::Pending)], &GATES)?;
        Ok(RunSummary {
            verdict: verdict.status,
            failures: verdict.failures.len(),
            cleanup: Ok(()),
            rfc,
            bundle: out_dir.to_path_buf(),
        })
    }
}
