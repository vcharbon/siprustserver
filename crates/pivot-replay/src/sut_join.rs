//! What joins a hosted-lane cell to the SUT's own records (`sut-join.json`):
//! the Call-IDs the cell minted, which the SUT keys its call state and its log
//! lines by, and the wall-clock window the cell drove it in, which slices the
//! SUT's log.

use std::path::Path;

use pivot_interpreter::DialogIdentity;
use pivot_schema::PivotV3;
use serde::{Deserialize, Serialize};

use crate::Failure;

/// The dialog identity every call of `document` is placed under in the cell
/// whose identity nonce is `nonce`, in document order: what its caller leg
/// dials with.
pub fn minted(document: &PivotV3, nonce: &str) -> Vec<DialogIdentity> {
    document
        .calls
        .iter()
        .map(|call| pivot_interpreter::dialog_identity(&call.caller_leg, nonce))
        .collect()
}

/// The bundle file the join is written to.
pub const FILE: &str = "sut-join.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SutJoin {
    /// The Call-ID of every call the cell placed, in document order.
    pub call_ids: Vec<String>,
    /// Unix milliseconds the interpreter started driving.
    pub started_at_ms: i64,
    /// Unix milliseconds it stopped.
    pub ended_at_ms: i64,
}

/// Writes `join` into the bundle at `out_dir`.
pub fn record(out_dir: &Path, join: &SutJoin) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(join)
        .map_err(|e| Failure::Environment(format!("serialize {FILE}: {e}")))?;
    std::fs::write(out_dir.join(FILE), text + "\n")
        .map_err(|e| Failure::Environment(format!("write {}/{FILE}: {e}", out_dir.display())))
}

#[cfg(test)]
#[path = "sut_join_tests.rs"]
mod tests;
