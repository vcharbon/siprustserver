//! What a wrapping harness reads of the dialog verdicts its UAs recorded:
//! the parties it judges, and each verdict a `dialog-…` waiver covered with
//! the waiver that did.

use super::dialog_gate::classify;
use super::harness::Harness;
use crate::dialogs::DialogVerdict;
use crate::WaiverScope;

impl Harness {
    /// The dialog verdicts this run's UAs recorded that a `dialog-…` waiver
    /// covers, each with the UA that recorded it and the first waiver
    /// covering it, in registration order.
    pub fn waived_dialog_verdicts(&self) -> Vec<(String, DialogVerdict, WaiverScope)> {
        let waivers = self.waivers.borrow();
        classify(&self.judges.borrow(), &waivers)
            .into_iter()
            .filter_map(|(g, w)| Some((g.party, g.verdict, waivers[w?].scope.clone())))
            .collect()
    }

    /// The UAs whose dialogs this run judges, in the order they were bound.
    pub fn judged_parties(&self) -> Vec<String> {
        let mut parties: Vec<String> = Vec::new();
        for judge in self.judges.borrow().iter() {
            if judge.is_on() && !parties.iter().any(|p| p == judge.party()) {
                parties.push(judge.party().to_string());
            }
        }
        parties
    }
}
