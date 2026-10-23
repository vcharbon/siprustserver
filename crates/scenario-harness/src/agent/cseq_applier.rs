//! The applier a scenario's declared CSeq pattern runs through, shared by a
//! dialog and the actor that drives it.

use std::sync::{Arc, Mutex};

use sip_message::{CseqDeviation, CseqPattern};

use super::dialog::SharedCseqDev;

/// The shared applier of `pattern`. A pattern that declares two ops at one
/// step is a scenario defect, and fails the scenario with its reason.
pub(crate) fn shared_cseq_applier(pattern: CseqPattern) -> SharedCseqDev {
    let applier = CseqDeviation::new(pattern).unwrap_or_else(|e| panic!("{e}"));
    Arc::new(Mutex::new(applier))
}
