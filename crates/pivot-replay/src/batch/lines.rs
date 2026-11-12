//! The batch's protocol lines on stdout, one JSON object each: `ready`,
//! `cell` and `end`. A product pins every shape it writes beside its crate,
//! where its batch client's suite decodes the same file.

use serde::Serialize;

use super::End;

/// One cell's line on the batch's stdout.
#[derive(Debug, Serialize)]
pub struct CellLine {
    /// The run-spec path as it came in.
    pub spec: String,
    /// The `replay <run-spec>` exit vocabulary.
    pub exit: i32,
    /// The summary a `replay <run-spec>` prints, or why the cell produced none.
    pub summary: String,
    /// The lines the cell wrote on stderr (`crate::diag`), its own and no
    /// other cell's.
    pub stderr: String,
}

/// The `ready` line: where the backend double is served (`bl`), the
/// descriptor limit the process runs with, the batch's seed.
pub fn ready_line(backend: &str, nofile: &str, seed: u64) -> serde_json::Value {
    serde_json::json!({ "ready": { "bl": backend, "nofile": nofile, "seed": format!("{seed:016x}") } })
}

/// One cell's line.
pub fn cell_line(line: &CellLine) -> serde_json::Value {
    serde_json::json!({ "cell": line })
}

/// The batch's `end` line.
pub fn end_line<R: Serialize>(end: &End<R>) -> serde_json::Value {
    serde_json::json!({ "end": end })
}
