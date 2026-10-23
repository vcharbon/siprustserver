//! In-memory CDR writer — the test sink. Records are appended on `write` and
//! read back via `read_all` to assert one CDR per terminated call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::time::Instant;

use call::Call;

use super::{build_record, CdrRecord, CdrWriter};
use crate::poison::{locked, CDR_MEMORY};

#[derive(Clone, Default)]
pub struct InMemoryCdrWriter {
    records: Arc<Mutex<Vec<CdrRecord>>>,
}

impl InMemoryCdrWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous snapshot (for tests that already hold a handle).
    pub fn snapshot(&self) -> Vec<CdrRecord> {
        locked(&self.records, CDR_MEMORY).clone()
    }
}

#[async_trait]
impl CdrWriter for InMemoryCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        locked(&self.records, CDR_MEMORY).push(build_record(call, terminated_at));
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        locked(&self.records, CDR_MEMORY).clone()
    }

    async fn close(&self, _deadline: Instant) {}
}
