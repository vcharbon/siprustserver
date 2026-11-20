//! What a dead hop records before the rules read the event: the INVITE that
//! dialled the leg drew no response of any kind (RFC 3261 §17.1.1.2), so the
//! leg waits on nothing from it ([`call::Leg::is_resolved`]), until a
//! response to that INVITE shows the hop alive after all.

use b2bua_sdk::event::CallEvent;
use call::{Call, TxnTimeoutKind};
use sip_message::{Method, SipMessage};

/// Record on `leg` that its dialling INVITE's hop is dead when `event` is the
/// `Response` timeout of an INVITE: every response, a 100 included, moves the
/// transaction past that timeout, so it fires only on a hop that answered
/// nothing. A response to the INVITE records the hop alive again.
pub(super) fn record(call: &mut Call, leg: &str, event: &CallEvent) {
    match event {
        CallEvent::Timeout { timeout_kind: TxnTimeoutKind::Response, method, cseq, .. }
            if method.as_deref() == Some(Method::Invite.as_str()) =>
        {
            call::helpers::record_dead_hop(call, leg, i64::from(*cseq));
        }
        CallEvent::Sip { message, .. } => {
            if let SipMessage::Response(resp) = message.as_ref() {
                if resp.cseq().method() == Method::Invite {
                    let cseq = i64::from(resp.cseq().seq());
                    call::helpers::record_hop_answered(call, leg, cseq);
                }
            }
        }
        _ => {}
    }
}
