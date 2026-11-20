//! One cell of the batch, run on its own thread, as the protocol line it ends
//! with: the product's cell run ([`Registered::cell`]), its panic its own,
//! its stderr captured.

use std::path::Path;

use super::{CellLine, Host, Registered};
use crate::lanes::panic_text;
use crate::{exit_code, Failure, RunSummary};

/// Runs the cell whose run-spec is at `path`, the batch's `ordinal`-th.
pub fn run_cell<P: Registered>(host: &Host<P>, path: &str, ordinal: u64) -> CellLine {
    line_of(path, || P::cell(host, Path::new(path), ordinal))
}

/// The protocol line of `cell`, run as the cell whose run-spec is at `path`.
pub fn line_of(path: &str, cell: impl FnOnce() -> Result<RunSummary, Failure>) -> CellLine {
    let (result, stderr) = crate::diag::captured(|| guarded(cell));
    let summary = match &result {
        Ok(summary) => crate::summary_line(path, summary),
        Err(failure) => failure.to_string(),
    };
    CellLine { spec: path.to_string(), exit: exit_code(&result), summary, stderr }
}

/// `cell` with a panic anywhere in it turned into the cell's own
/// `Failure::Panicked`.
pub fn guarded(cell: impl FnOnce() -> Result<RunSummary, Failure>) -> Result<RunSummary, Failure> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(cell))
        .unwrap_or_else(|panic| Err(Failure::Panicked(panic_text(panic.as_ref()))))
}
