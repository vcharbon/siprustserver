//! The **run-bundle contracts**: the record kinds one interpreter run leaves on
//! disk, and that every reader decodes.
//!
//! ```text
//! <run-dir>/
//!   pivot.json              the document that ran, canonically formatted
//!   run-config.json         `RunConfig` — the lane's compiled configuration
//!   recording/<leg>.jsonl   one `RecordedMessage` per line, in wire order
//!   verdict.json            `RunVerdict` — what the run decided, and why
//!   timing.json             `RunTiming` — when it started and when it settled
//!   rfc.json                `RunRfcAudit` — the post-run RFC audit, or that none ran
//! ```
//!
//! They sit beside the document contract because ONE crate owns every wire
//! type: the interpreter imports its own output shapes, and one binary emits
//! every schema a mirror in another language is checked against. What PRODUCES
//! a bundle — the in-memory recording handle, the writer armed before a run
//! body — is replay machinery and stays in the interpreter.
//!
//! [`RunTiming`] is the run's own clock; [`crate::document::Timing`] is the
//! document's expect and settle budgets. Different facts, so different names.

pub mod bindings;
pub mod recording;
pub mod rfc;
pub mod runconfig;
pub mod timing;
pub mod verdict;

pub use bindings::{BindingError, IdentityBindings};
pub use recording::{Dir, RecordedMessage};
pub use rfc::{RfcFinding, RunRfcAudit};
pub use runconfig::{CheckDisposition, ClockMode, RunConfig};
pub use timing::RunTiming;
pub use verdict::{
    Abandoned, Arrived, CloseAct, CloseOwed, DeclaredNote, Failure, GatedOn, Informative,
    LadderSide, RetransmitNote, RunVerdict, TimingNote, VerdictStatus, ViolationNote, Waived,
};
