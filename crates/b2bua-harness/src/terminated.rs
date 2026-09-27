//! A [`CdrWriter`] that keeps every terminated `Call` as the record saw it —
//! the message rings, the decision log and the termination record, which the
//! `CdrRecord` projection does not carry whole — while writing the record as
//! [`InMemoryCdrWriter`] does.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter, InMemoryCdrWriter};
use call::Call;

/// The terminated calls a writer kept, in write order. Clones share one list.
#[derive(Clone, Default)]
pub struct TerminatedCalls(Arc<Mutex<Vec<Call>>>);

impl TerminatedCalls {
    pub fn snapshot(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
}

/// Writes each record into an [`InMemoryCdrWriter`] and, when it keeps them,
/// pushes the `Call` the record was built from onto a [`TerminatedCalls`]
/// list, the `Call` first; a tap, when set, is then handed the same write.
#[derive(Clone)]
pub struct TerminatedCallsWriter {
    records: InMemoryCdrWriter,
    terminated: Option<TerminatedCalls>,
    tap: Option<Arc<dyn CdrWriter>>,
}

impl TerminatedCallsWriter {
    /// A writer keeping every terminated `Call` on `terminated`.
    pub fn new(terminated: TerminatedCalls) -> Self {
        Self { records: InMemoryCdrWriter::new(), terminated: Some(terminated), tap: None }
    }

    /// A writer keeping the records only, as [`InMemoryCdrWriter`] does.
    pub fn records_only() -> Self {
        Self { records: InMemoryCdrWriter::new(), terminated: None, tap: None }
    }

    /// This writer, handing every write to `tap` after its own, within the
    /// same write.
    pub fn with_tap(self, tap: Arc<dyn CdrWriter>) -> Self {
        Self { tap: Some(tap), ..self }
    }

    /// The records written so far.
    pub fn records(&self) -> Vec<CdrRecord> {
        self.records.snapshot()
    }

    /// The terminated calls kept so far; `None` from a records-only writer.
    pub fn terminated_calls(&self) -> Option<Vec<Call>> {
        self.terminated.as_ref().map(TerminatedCalls::snapshot)
    }
}

#[async_trait]
impl CdrWriter for TerminatedCallsWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        if let Some(terminated) = &self.terminated {
            terminated.0.lock().unwrap().push(call.clone());
        }
        self.records.write(call, terminated_at).await;
        if let Some(tap) = &self.tap {
            tap.write(call, terminated_at).await;
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.records.read_all().await
    }
}
