//! What a rule reads of a `Timeout` event: the request of this stack that
//! drew no final, and which deadline its client transaction gave up on.

use call::TxnTimeoutKind;

use crate::event::CallEvent;
use crate::model::RuleContext;

impl RuleContext<'_> {
    /// For a `Timeout` event: the method of the request that drew no final.
    /// `None` for every other event kind.
    pub fn timeout_method(&self) -> Option<&str> {
        match self.event {
            CallEvent::Timeout { method, .. } => method.as_deref(),
            _ => None,
        }
    }
    /// For a `Timeout` event: which client-transaction timeout fired —
    /// `Response` (nothing at all answered: the hop is dead) or `Transaction`
    /// (it answered a provisional, then went silent past the INVITE bound).
    /// `None` for every other event kind.
    pub fn timeout_kind(&self) -> Option<TxnTimeoutKind> {
        match self.event {
            CallEvent::Timeout { timeout_kind, .. } => Some(*timeout_kind),
            _ => None,
        }
    }
}
