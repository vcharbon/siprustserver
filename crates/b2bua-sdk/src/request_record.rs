//! What a rule reads of the request record its event settled (ADR-0055 §2):
//! the one a response answers, or a transaction timeout ends, as the engine
//! resolved it before the rules.

use crate::event::CallEvent;
use crate::model::RuleContext;

impl<'a> RuleContext<'a> {
    /// The request record the current response answers, as the engine's
    /// dialog gate resolved it ([`Self::resolved`]): kept on the dialog the
    /// response's tags name, or among the requests a retired dialog still
    /// holds — never another dialog's. `None` for any non-response event and
    /// for a response to nothing this stack recorded.
    pub fn answered_record(&self) -> Option<&'a call::RequestRecord> {
        self.response()?;
        self.resolved.record.as_deref().map(|(_, r)| r)
    }

    /// The relay the current response answers ([`Self::answered_record`]
    /// when it records a relay). `None` for a request this stack originated
    /// itself and for any non-response event.
    pub fn relayed_pending(&self) -> Option<call::RequestRecord> {
        self.answered_record().filter(|r| r.is_relay()).cloned()
    }

    /// Does the current response answer a request THIS STACK RELAYED
    /// ([`Self::relayed_pending`])? `false` for a request the B2BUA originated
    /// itself (keepalive OPTIONS, its own PRACK, a REFER-progress NOTIFY) and
    /// for any non-response event.
    pub fn answers_relayed_request(&self) -> bool {
        self.relayed_pending().is_some()
    }

    /// For a `Timeout` event: the request record the timeout settled, as it
    /// stood when its transaction gave up (ADR-0055 §2). `None` for every
    /// other event kind, and for a transaction no record names (an INVITE).
    pub fn timed_out_record(&self) -> Option<&'a call::RequestRecord> {
        matches!(self.event, CallEvent::Timeout { .. })
            .then(|| self.resolved.record.as_deref().map(|(_, r)| r))
            .flatten()
    }
}
