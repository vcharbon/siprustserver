//! The end of a call served from its record, still waiting on its first
//! routing decision, whose decision is not asked again (ADR-0047 D4, D6): its
//! caller refused 503 when she waits on a transaction this node holds, else
//! ended with nothing on the wire. A setup the caller CANCELed during its
//! consult ends under her cancel, its CDR stating it as the CANCEL's own turn
//! does.

use call::{Call, CallModelState, CdrEvent, CdrEventType, TerminationCause};
use sip_message::generators::CapabilitySet;

use crate::effects::HandlerResult;
use crate::turn_scope::TurnScope;

/// How an undecided call ended.
pub enum Settled {
    /// Its caller refused 503 under the deployment's minted-final
    /// advertisement.
    Refused(HandlerResult),
    /// Ended with nothing on the wire.
    Ended(HandlerResult),
}

/// End the undecided `call` at `now_ms`; `answer` when its caller waits on
/// the INVITE transaction this node holds and did not CANCEL. A caller's
/// INVITE that does not rebuild is not answered.
pub fn settle(
    call: Call,
    answer: bool,
    advertisement: &CapabilitySet,
    turn: &mut TurnScope,
    now_ms: i64,
) -> Settled {
    let caller = answer.then(|| crate::rules::relay::caller_invite(&call, &mut turn.notes));
    if let Some(a_invite) = caller.flatten() {
        return Settled::Refused(crate::initial_invite::reject_call(
            call,
            &a_invite,
            503,
            Some("Service Unavailable".into()),
            None,
            &[],
            advertisement,
            now_ms,
            TerminationCause::Admission,
        ));
    }
    let (cause, by_leg) = if call.setup_cancelled {
        (TerminationCause::RemoteCancel, Some(call.a_leg.leg_id.clone()))
    } else {
        (TerminationCause::Admission, None)
    };
    let mut ended = call;
    if ended.setup_cancelled {
        let event = CdrEvent {
            event_type: CdrEventType::Cancel,
            timestamp: now_ms,
            leg_id: ended.a_leg.leg_id.clone(),
            status_code: None,
            reason: None,
            decision_ordinal: 0,
        };
        call::helpers::add_cdr_event(&mut ended, event);
    }
    call::helpers::record_termination(&mut ended, now_ms, cause, by_leg);
    ended.state = CallModelState::Terminated;
    Settled::Ended(HandlerResult::new(ended))
}
