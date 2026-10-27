//! A 100 its INVITE's final stood in for (RFC 3261 §17.2.1): the expect is
//! retired as omitted, the omission named once, and the final offered on to
//! the step behind it (`progress`).

use pivot_schema::bundle::{Failure, RecordedMessage};
use tokio::time::Instant;

use super::Runner;
use crate::gate::Inbound;
use crate::plan::CompiledStep;
use crate::progress;

impl<'a, 'p> Runner<'a, 'p> {
    /// Retire `step`, the `100` the final `inbound` stood in for, release the
    /// tolerated absences armed on its transaction, and name the omission with
    /// the final's arrival measured from the INVITE's emission. `false`, and
    /// nothing done, where the step is not retirable.
    ///
    /// `ladder` is the leg's recording with the final in it, `seq` the final's
    /// place there.
    pub(super) fn omit_trying(
        &mut self,
        leg: &str,
        step: &CompiledStep,
        inbound: &Inbound,
        seq: Option<u64>,
        ladder: &[RecordedMessage],
        released: &[String],
    ) -> bool {
        let now = Instant::now();
        if !self.instance.retire_step(&step.id) {
            return false;
        }
        self.completed_at.insert(step.id.clone(), now);
        for id in released {
            if self.instance.release_step(id) {
                self.completed_at.insert(id.clone(), now);
            }
        }
        let arrived_us = ladder
            .iter()
            .find(|recorded| Some(recorded.seq) == seq)
            .map_or_else(|| self.now_us(), |recorded| recorded.at_us);
        let sent_us = progress::sent_at(ladder, inbound).unwrap_or(arrived_us);
        self.instance.fail(Failure::TryingOmitted {
            step: step.id.clone(),
            leg: leg.to_string(),
            arrived: inbound.arrived(),
            after_ms: arrived_us.saturating_sub(sent_us) / 1000,
        });
        if let Some(seq) = seq {
            let note = format!(
                "the final of the INVITE transaction {} waits on a 100 for: an INVITE server \
                 transaction answering at once sends no 100 (RFC 3261 §17.2.1); {} retired as \
                 omitted",
                step.id, step.id
            );
            self.instance.recording().renote(leg, seq, &note);
        }
        self.settle_blocks(now);
        true
    }
}
