//! The dialog tables a harness judges, one per UA it binds, each reading
//! from the run's recording what the wire shows beyond the UA's own sight.

use std::sync::Arc;

use super::Harness;
use crate::dialogs::{AheadOfFinal, DialogJudge, LostOnTheWay, SentBeforeEnd};

impl Harness {
    /// A harness-judged dialog table for the UA `party`, gated at
    /// [`finish`](Self::finish). On a fabric that runs a loss model it reads
    /// from the recording the losses that explain a CSeq out of sequence,
    /// the requests that crossed the final or the BYE that ended their
    /// dialog, and the requests of the UA that reached the peer ahead of its
    /// own final; on any other run no loss explains a verdict.
    pub(crate) fn dialog_judge(&self, party: &str) -> Arc<DialogJudge> {
        let mut judge = DialogJudge::harnessed(party, self.dialog_opt_ins.clone());
        if self.lossy {
            judge = judge.explaining_losses(
                LostOnTheWay::from_wire(self.recording.channel()),
                SentBeforeEnd::from_wire(self.recording.channel()),
                AheadOfFinal::from_wire(self.recording.channel()),
            );
        }
        let judge = Arc::new(judge);
        self.judges.borrow_mut().push(judge.clone());
        judge
    }
}
