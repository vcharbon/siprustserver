//! The TERMINATED CALLS of one run, and the `call.terminated.json` they land
//! in beside `verdict.json`: every `Call` the SUT's call-record writer
//! received by the time the harness settled the fabric, whole and in write
//! order, the input a product maps its call records from. A run that unwinds
//! leaves the calls terminated when the interpreter stopped.

use std::path::Path;

use call::Call;

use crate::Failure;

/// The bundle file holding this run's terminated calls.
pub const TERMINATED_FILE: &str = "call.terminated.json";

/// `call.terminated.json`'s text for `calls`: a JSON array, one `Call` each.
pub fn text(calls: &[Call]) -> Result<String, Failure> {
    serde_json::to_string_pretty(calls)
        .map(|text| text + "\n")
        .map_err(|e| Failure::Environment(format!("serialize terminated calls: {e}")))
}

/// Write `call.terminated.json` into the bundle.
pub fn record(out_dir: &Path, calls: &[Call]) -> Result<(), Failure> {
    std::fs::write(out_dir.join(TERMINATED_FILE), text(calls)?)
        .map_err(|e| Failure::Environment(format!("write {TERMINATED_FILE}: {e}")))
}
