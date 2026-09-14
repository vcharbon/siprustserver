//! A [`CdrWriter`] that hands the test the terminated `Call` as the record saw
//! it — the message rings and the decision log, which the `CdrRecord`
//! projection does not carry whole — while writing the record as usual.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter, InMemoryCdrWriter};
use call::Call;

/// The terminated calls the SUT wrote, as the record saw them.
#[derive(Clone, Default)]
pub struct TerminatedCalls(Arc<Mutex<Vec<Call>>>);

impl TerminatedCalls {
    pub fn snapshot(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
}

pub struct ProbeCdr {
    inner: InMemoryCdrWriter,
    terminated: TerminatedCalls,
}

impl ProbeCdr {
    pub fn new(terminated: TerminatedCalls) -> Self {
        Self { inner: InMemoryCdrWriter::new(), terminated }
    }
}

#[async_trait]
impl CdrWriter for ProbeCdr {
    async fn write(&self, call: &Call, terminated_at: i64) {
        self.terminated.0.lock().unwrap().push(call.clone());
        self.inner.write(call, terminated_at).await;
    }
    async fn read_all(&self) -> Vec<CdrRecord> {
        self.inner.read_all().await
    }
}
