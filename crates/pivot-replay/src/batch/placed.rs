//! One cell's calls as the batch placed them: what joins a state the SUT
//! keeps per call, and an exchange of the backend double, to the cell that
//! placed the call.

use std::path::PathBuf;

use pivot_interpreter::DialogIdentity;

/// One cell's calls: its bundle and the dialog identity each call was seeded
/// with.
#[derive(Debug, Clone)]
pub struct Placed {
    pub bundle: PathBuf,
    pub seeds: Vec<DialogIdentity>,
}

impl Placed {
    /// The a-leg Call-ID of each call, in document order.
    pub fn call_ids(&self) -> Vec<String> {
        self.seeds.iter().map(|seed| seed.call_id.clone()).collect()
    }
}
