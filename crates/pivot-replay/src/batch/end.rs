//! The batch's end, once every cell ended ([`Host::finish`]): the backend
//! double's last exchange waited out, the registered gates
//! ([`Registered::end`]), every recorded cell handed back to the product,
//! and every cell's lines the end answers settled. Each step names itself in
//! the batch's wait line (`waiting`) before it starts, each wait with its
//! bound.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use super::gates::{dirty_of, Gate};
use super::{served, BackendDouble, Host, Registered, Served};
use crate::verifications::{self, Standing};

/// How long the batch waits for a relaying backend double's last exchange
/// once every cell ended.
pub const BACKEND_SETTLE: Duration = Duration::from_secs(2);

/// What the registered gates of the batch's end came to.
#[derive(Debug)]
pub struct Ended<R> {
    /// Each gate the lane wires, answered apart, in the order the end read them.
    pub gates: Vec<Gate>,
    /// The bundles whose verdict a gate charged.
    pub charged: Vec<PathBuf>,
    /// How the end stands on each line it answers for every cell placed.
    pub settled: Vec<(&'static str, Standing)>,
    /// What the `end` line states beside the gates.
    pub report: R,
}

/// What the batch's end came to, as the `end` line states it.
#[derive(Debug, Serialize)]
pub struct End<R> {
    /// What the SUT still held once every cell ended, `None` when clean: the
    /// failing gates' findings ([`dirty_of`]).
    pub dirty: Option<String>,
    /// Each gate the lane wires, answered apart, in the order the end read them.
    pub gates: Vec<Gate>,
    /// What the product's gates state beside them.
    #[serde(flatten)]
    pub report: R,
    /// The bundles whose verdict a gate charged.
    pub charged: Vec<PathBuf>,
    /// What the backend double served over the batch, and how fast.
    #[serde(rename = "bl")]
    pub backend: Served,
}

impl<P: Registered> Host<P> {
    /// The batch's end, once every cell ended.
    pub async fn finish(&self) -> End<P::Report> {
        if self.backend.relays() {
            self.waiting.step(format!(
                "the relay's last exchange (at most {} s)",
                BACKEND_SETTLE.as_secs()
            ));
            if !self.backend.settled(BACKEND_SETTLE).await {
                eprintln!("replay batch: the relay still carries an exchange at batch end");
            }
        }
        let placed = self.placed();
        let Ended { gates, charged, settled, report } = P::end(self, &placed).await;
        let dirty = dirty_of(&gates);
        let times: Vec<Option<u64>> =
            self.backend.rows().iter().map(P::Backend::process_us).collect();
        let backend = served::of(&times);
        let finished =
            std::mem::take(&mut *self.finished.lock().unwrap_or_else(|p| p.into_inner()));
        P::recorded(self, finished);
        self.waiting.step(format!("every cell's lines the end answers ({} cell(s))", placed.len()));
        for cell in &placed {
            if let Err(why) = verifications::settle(&cell.bundle, &settled, P::gates()) {
                eprintln!("replay batch: {}: lines not settled: {why}", cell.bundle.display());
            }
        }
        self.waiting.done();
        End { dirty, gates, report, charged, backend }
    }
}
