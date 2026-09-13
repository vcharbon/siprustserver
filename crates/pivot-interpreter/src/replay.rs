//! The crate's **front door**: one call that owns a whole run.
//!
//! Everything a run must sequence correctly to leave an honest bundle lives
//! here — compile the document, arm the bundle writer BEFORE the run body,
//! drive the executor, hand the writer the verdict, commit the bundle — so no
//! caller can ship a bundle claiming `RunUnwound` by sequencing it wrong.
//! What the caller still owns is lane knowledge (`PCAP2TEST_PIVOT_V3.md`
//! §4.3): the identity bindings, the URI composer, the bound agents, and the
//! run configuration.

use std::io;
use std::path::PathBuf;

use pivot_schema::bundle::RunConfig;
use pivot_schema::PivotV3;

use crate::bundle::BundleWriter;
use crate::exec::{self, Lane, Outcome};
use crate::plan::{Plan, PlanError};
use crate::recording::Recording;
use crate::settle::Sut;

/// Why [`replay`] produced no run.
#[derive(Debug)]
pub enum ReplayError {
    /// The document refused to compile; every refusal is listed. No bundle is
    /// written, because there is no run to record.
    Plan(Vec<PlanError>),
    /// The run finished but its bundle could not be written under `out_dir`.
    Bundle(io::Error),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Plan(errors) => {
                write!(f, "the document does not compile:")?;
                for error in errors {
                    write!(f, "\n  {error}")?;
                }
                Ok(())
            }
            ReplayError::Bundle(e) => write!(f, "the run bundle was not written: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReplayError::Plan(_) => None,
            ReplayError::Bundle(e) => Some(e),
        }
    }
}

/// Run `document` on `lane` against `sut`, leaving the run bundle under
/// `out_dir`.
///
/// `out_dir` is the run's record: whatever a previous run left there is
/// removed, never mixed into this bundle. A run body that panics still leaves
/// its ladder — the writer is armed before the body and writes on unwind,
/// under a verdict stating the run never returned. The bundle's run
/// configuration states the media mode the lane's booking ran, whatever
/// `config` said.
pub async fn replay(
    document: PivotV3,
    config: RunConfig,
    lane: Lane<'_>,
    sut: &dyn Sut,
    out_dir: impl Into<PathBuf>,
) -> Result<Outcome, ReplayError> {
    let out_dir = out_dir.into();
    let config = config.with_media(lane.media.mode());
    let case = document.case.id.clone();
    let canonical = document.to_canonical_json();
    let plan = Plan::compile(document).map_err(ReplayError::Plan)?;
    let _ = std::fs::remove_dir_all(&out_dir);
    let mut writer = BundleWriter::arm(
        &out_dir,
        &case,
        canonical,
        config.clone(),
        Recording::new(),
        plan.document().timing.settle_budget_ms,
    );
    let outcome = exec::run_recording_into(&plan, config, lane, sut, writer.recording()).await;
    writer.finish(outcome.verdict.clone(), outcome.timing);
    writer.commit().map_err(ReplayError::Bundle)?;
    Ok(outcome)
}
