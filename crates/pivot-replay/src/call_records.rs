//! The `call-records` export of a simulated system whose worker writes the
//! B2BUA's own record per call ([`CdrRecord`]): each record a line keyed by
//! its a-leg's Call-ID, the Call-ID the caller sent, so the base joins it to
//! the cell whose `call_id` key holds it (ADR-0060 appendix §4, §6).

use std::path::{Path, PathBuf};

use b2bua::cdr::CdrRecord;
use pivot_interpreter::records::CALL_RECORDS;

use crate::exports::{self, Line};
use crate::Failure;

/// The export line of `record`: the record as the worker wrote it, keyed by
/// its a-leg's Call-ID.
pub fn line_of(record: &CdrRecord) -> Result<Line, Failure> {
    let value = serde_json::to_value(record)
        .map_err(|e| Failure::Environment(format!("a call record does not serialize: {e}")))?;
    Ok(Line::Keyed { record: value, key: record.a_leg.call_id.clone() })
}

/// Writes `records` as the `call-records` export of the cell whose directory
/// is `out_dir`, one keyed line each.
pub fn export(out_dir: &Path, records: &[CdrRecord]) -> Result<PathBuf, Failure> {
    let lines = records.iter().map(line_of).collect::<Result<Vec<Line>, Failure>>()?;
    exports::write(out_dir, CALL_RECORDS, &lines)
}
