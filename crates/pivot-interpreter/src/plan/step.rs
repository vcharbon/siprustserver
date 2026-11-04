//! A compiled flow step: what the run reads of one message step, resolved once.

use pivot_schema::check::Check;
use pivot_schema::flow::{CheckMode, Delay};
use pivot_schema::msg::MsgSpec;

use super::Discriminator;
use crate::program::StepLoc;

/// What a compiled step does on the wire, carrying each side's own fields —
/// so an expect's check mode is mandatory here, where the document's `Option`
/// has already been refused if absent, and `optional` cannot exist on a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// The actor emits the message.
    Send,
    /// The actor waits for the message, within its budget.
    Expect {
        /// Whether the stored content is matched or merely recorded.
        check: CheckMode,
        /// Tolerated absence (§6.5).
        optional: bool,
    },
}

/// One flow step, compiled: every field the run needs, resolved once.
#[derive(Debug, Clone)]
pub struct CompiledStep {
    pub id: String,
    pub leg: String,
    pub kind: StepKind,
    pub auto: bool,
    pub after: Vec<String>,
    pub checks: Vec<Check>,
    pub early: Option<String>,
    pub overlap: Option<String>,
    /// A send's awaited lane event (§6.7g); `None` on every expect.
    pub awaits: Option<String>,
    pub msg: MsgSpec,
    pub delay: Delay,
    /// The step's own budget: its `within_ms` where it states one, else
    /// `timing.expect_budget_ms`.
    pub within_ms: u64,
    pub retransmits: Option<u32>,
    /// The document's own pacing for those repeats, one entry per rung. Empty
    /// where the document states none and the RFC's ladder paces instead.
    pub retransmit_intervals_ms: Vec<u64>,
    pub(crate) loc: StepLoc,
    /// Document order, over every step the flow holds.
    pub order: usize,
    /// Indices into `document.deviations` of the entries that apply to this step.
    pub deviations: Vec<usize>,
    pub discriminator: Discriminator,
}

impl CompiledStep {
    /// Whether the run may compress this step's dwell on a virtual clock. Read
    /// from the document and never re-derived.
    pub fn compressible(&self) -> bool {
        self.delay.compressible
    }

    pub fn is_send(&self) -> bool {
        matches!(self.kind, StepKind::Send)
    }

    pub fn is_expect(&self) -> bool {
        matches!(self.kind, StepKind::Expect { .. })
    }

    /// A tolerated absence: an `optional` expect (§6.5).
    pub fn optional_expect(&self) -> bool {
        matches!(self.kind, StepKind::Expect { optional: true, .. })
    }

    /// Whether the stored content is MATCHED against the inbound message
    /// (`check: assert`) rather than merely recorded.
    pub fn asserts_content(&self) -> bool {
        matches!(self.kind, StepKind::Expect { check: CheckMode::Assert, .. })
    }
}
