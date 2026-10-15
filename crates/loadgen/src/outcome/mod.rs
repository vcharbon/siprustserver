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
