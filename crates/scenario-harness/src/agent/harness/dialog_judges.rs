//! The dialog tables a harness judges, one per UA it binds, each reading
//! from the run's recording what the wire shows beyond the UA's own sight.

use std::sync::Arc;

use super::Harness;
use crate::dialogs::{DialogJudge, LostOnTheWay, SentBeforeBye};

impl Harness {
    /// A harness-judged dialog table for the UA `party`, gated at
    /// [`finish`](Self::finish), reading BYE crossings from the recording,
    /// and on a fabric that runs a loss model the losses that explain a CSeq
    /// out of sequence.
    pub(crate) fn dialog_judge(&self, party: &str) -> Arc<DialogJudge> {
        let wire = SentBeforeBye::from_wire(self.recording.channel());
        let mut judge = DialogJudge::harnessed(party, self.dialog_opt_ins.clone(), wire);
        if self.lossy {
            judge = judge.explaining_losses(LostOnTheWay::from_wire(self.recording.channel()));
        }
        let judge = Arc::new(judge);
        self.judges.borrow_mut().push(judge.clone());
        judge
    }
}
