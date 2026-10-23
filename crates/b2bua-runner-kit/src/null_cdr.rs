//! The discarding CDR sink, the default when a runner wires no external store.

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter};
use call::Call;
use tokio::time::Instant;

/// A CDR sink that discards every record. The default sink when a runner wires
/// no external CDR store — for load/endurance the process must not accumulate
/// records in memory. Always wrapped by `BufferedCdrWriter` (see
/// [`RunnerBase::deps`](crate::RunnerBase::deps)) so the buffer/drainer
/// machinery is still exercised.
pub struct NullCdrWriter;

#[async_trait]
impl CdrWriter for NullCdrWriter {
    async fn write(&self, _call: &Call, _terminated_at: i64) {}
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }
    async fn close(&self, _deadline: Instant) {}
}
