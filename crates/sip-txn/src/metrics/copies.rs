//! The copies the layer absorbed past the RFC schedule (ADR-0063 §1), by
//! what they copied.

use super::TransactionMetrics;
use crate::layer::InviteRefusals;

/// What a copy absorbed past its schedule copied, and so which answer it
/// went without.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    /// A request a server transaction holds: its last response, a 481 to a
    /// CANCEL matching no INVITE included.
    Request,
    /// A CANCEL of an INVITE that took its final: its 200.
    Cancel,
    /// A non-2xx final to an INVITE this layer sent: its hop ACK.
    Non2xx,
    /// A 2xx to an orphaned INVITE (ADR-0034): its dialog's ACK.
    Final2xx,
    /// An INVITE the node refused before any transaction held it: the
    /// refusal.
    Refused,
}

impl CopyKind {
    /// Every kind, in label order.
    pub const ALL: [CopyKind; 5] = [
        CopyKind::Request,
        CopyKind::Cancel,
        CopyKind::Non2xx,
        CopyKind::Final2xx,
        CopyKind::Refused,
    ];

    /// The `kind` label value.
    pub const fn label(self) -> &'static str {
        match self {
            CopyKind::Request => "request",
            CopyKind::Cancel => "cancel",
            CopyKind::Non2xx => "non-2xx",
            CopyKind::Final2xx => "2xx",
            CopyKind::Refused => "refused-invite",
        }
    }

    /// The kind's position in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        match self {
            CopyKind::Request => 0,
            CopyKind::Cancel => 1,
            CopyKind::Non2xx => 2,
            CopyKind::Final2xx => 3,
            CopyKind::Refused => 4,
        }
    }
}

impl TransactionMetrics {
    /// Copies of `kind` absorbed past the RFC schedule (counter). The
    /// refused INVITEs' count is the node's shared refusals', every stage
    /// that answers from them included.
    pub fn copies_past_schedule(&self, kind: CopyKind) -> u64 {
        match kind {
            CopyKind::Refused => self.refusals.as_ref().map_or(0, InviteRefusals::copies_absorbed),
            _ => self.inner.copies_past_schedule.get(kind.index()),
        }
    }
}
