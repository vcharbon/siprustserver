//! Per-call outcome records (`call-outcome.v1`): one record for each call
//! loadgen placed, written when the call is done whatever path it took, stating
//! what the caller (and each callee leg loadgen played) saw on the wire. A call
//! never placed writes no record; it is counted per stream as not placed.
//!
//! - `wire` — the facts, from the call's mux wire tap.
//! - `record` — the record, from the facts and loadgen's own result.
//! - `sink` — the file, the VictoriaLogs push and the counters.

pub mod record;
pub mod sink;
pub mod wire;

pub use record::{build, RecordMeta};
pub use sink::{LogsTarget, OutcomeSink};
pub use wire::{CallWire, WireFacts};

use crate::class::CallOutcome;

/// Write the record of a call that ran into `sink`; a call whose scenario sent
/// no INVITE wrote none, a generator failure.
pub fn write_call(
    sink: &OutcomeSink,
    shape: &str,
    wire: &WireFacts,
    outcome: &CallOutcome,
    ended_at_ms: i64,
) {
    let meta = RecordMeta { run: &sink.run, source: &sink.source, stream: &sink.stream, shape };
    match build(&meta, wire, outcome, ended_at_ms) {
        Some(record) => sink.write(record),
        None => sink.generator_failure(shape, "no_invite"),
    }
}
