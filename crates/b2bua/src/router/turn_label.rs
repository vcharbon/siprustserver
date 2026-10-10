//! The label a turn's model-invariant violation names it by
//! ([`crate::rules::model_invariants::check`]): one line stating the event
//! the turn handled and its call.

use std::fmt;

use b2bua_sdk::event::CallEvent;
use sip_message::SipMessage;

/// A one-line summary of the event a turn handled, for the violation's
/// first line.
pub(super) struct EventSummary<'a> {
    pub(super) call_ref: &'a str,
    pub(super) event: &'a CallEvent,
}

impl fmt::Display for EventSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.event {
            CallEvent::Sip { message, src, .. } => match message.as_ref() {
                SipMessage::Request(r) => {
                    write!(f, "{} CSeq {} from {src}", r.method(), r.cseq().seq())?
                }
                SipMessage::Response(r) => write!(
                    f,
                    "{} to {} CSeq {} from {src}",
                    r.status(),
                    r.cseq().method(),
                    r.cseq().seq()
                )?,
            },
            CallEvent::Timer { timer_type, leg_id, .. } => {
                write!(f, "timer {timer_type:?} on {}", leg_id.as_deref().unwrap_or("the call"))?
            }
            CallEvent::Cancelled { invite_cseq, .. } => {
                write!(f, "CANCEL of INVITE CSeq {invite_cseq:?}")?
            }
            CallEvent::Timeout { method, leg_id, .. } => write!(
                f,
                "timeout of {} on {}",
                method.as_deref().unwrap_or("a request"),
                leg_id.as_deref().unwrap_or("the call")
            )?,
            CallEvent::InternalEvent { topic, outcome, .. } => {
                write!(f, "internal event {topic}/{outcome}")?
            }
            CallEvent::CallQuiesced { .. } => write!(f, "call quiesced")?,
        }
        write!(f, " on call {}", self.call_ref)
    }
}
