//! How the per-call dispatcher treats an event: how far past its bounds the
//! event may wait (the one-shot events nothing sends again, each bounded by
//! what produces it), and whether it counts toward the call's lifetime cap
//! (everything but the node's own work).

use call::{CallModelState, TimerType};
use sip_message::{Method, SipMessage};

use super::RouterCtx;
use crate::dispatch::Job;
use crate::event::CallEvent;

/// How far past the dispatcher's bounds an event's job may wait.
pub(super) enum Room {
    /// Dropped when there is no room.
    Bounded,
    /// Past a full queue and the cap, up to the call's overflow ceiling.
    PastBounds,
    /// Past every bound.
    PastAllBounds,
}

impl Room {
    /// The room `event` for `call_ref` needs.
    ///
    /// - A `Cancelled` — the layer already answered 200 + 487 and the caller
    ///   will not send it again — waits past a full queue and the cap, up to
    ///   the call's overflow ceiling; a call flooded past it is torn down by
    ///   the reaper.
    /// - A reaper verdict (paced by the sweep), the `TerminatingTimeout` fire
    ///   (one per termination), and, on a terminating call, a client
    ///   transaction's outcome — a final no retransmission brings back, or its
    ///   `Timeout` (one per transaction) — wait past every bound, so a
    ///   teardown completes even while a peer floods the call.
    pub(super) fn of(ctx: &RouterCtx, event: &CallEvent, call_ref: &str) -> Self {
        if crate::reaper::is_reaper_event(event)
            || matches!(event, CallEvent::Timer { timer_type: TimerType::TerminatingTimeout, .. })
            || (is_transaction_outcome(event)
                && ctx.state.model_state(call_ref) == Some(CallModelState::Terminating))
        {
            return Room::PastAllBounds;
        }
        if matches!(event, CallEvent::Cancelled { .. }) {
            return Room::PastBounds;
        }
        Room::Bounded
    }

    /// `job` admitted with this room.
    pub(super) fn admit(self, job: Job) -> Job {
        match self {
            Room::Bounded => job,
            Room::PastBounds => job.past_bounds(),
            Room::PastAllBounds => job.past_all_bounds(),
        }
    }
}

/// The node's own work — a timer it armed, or an event it raised itself (a
/// reaper verdict, a callout's result) — which the call's lifetime cap does
/// not count.
pub(super) fn is_own(event: &CallEvent) -> bool {
    matches!(event, CallEvent::Timer { .. } | CallEvent::InternalEvent { .. })
}

/// A client transaction's outcome the layer delivers once: its `Timeout`, or
/// a final it matched, other than a 2xx to an INVITE (the UAS retransmits
/// that one until it is ACKed, RFC 3261 §13.3.1.4).
fn is_transaction_outcome(event: &CallEvent) -> bool {
    match event {
        CallEvent::Timeout { .. } => true,
        CallEvent::Sip { message, matched_client_txn: true, .. } => match message.as_ref() {
            SipMessage::Response(r) => {
                r.status() >= 300 || (r.status() >= 200 && *r.cseq().method() != Method::Invite)
            }
            SipMessage::Request(_) => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use call::TimerType;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    use super::is_own;
    use crate::event::CallEvent;

    /// The lifetime cap counts what peers send, never the node's own timers
    /// or the events it raises itself.
    #[test]
    fn only_the_nodes_own_timers_and_events_are_its_own() {
        let timer = CallEvent::Timer {
            timer_type: TimerType::Keepalive,
            call_ref: "c".into(),
            leg_id: None,
        };
        let verdict = CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: crate::reaper::REAPER_TOPIC.into(),
            outcome: crate::reaper::OUTCOME_OVERFLOW.into(),
            payload: serde_json::json!({}),
            body: Vec::new(),
        };
        assert!(is_own(&timer) && is_own(&verdict));

        let raw = b"INFO sip:b2bua@127.0.0.1 SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-own\r\n\
            From: <sip:a@10.0.0.1>;tag=a\r\nTo: <sip:b@127.0.0.1>;tag=b\r\n\
            Call-ID: own@unit\r\nCSeq: 2 INFO\r\nContent-Length: 0\r\n\r\n";
        let message = CustomParser::new().parse(raw).unwrap();
        assert!(matches!(message, SipMessage::Request(_)));
        let sip = CallEvent::Sip {
            message: Box::new(message),
            src: "10.0.0.1:5060".parse().unwrap(),
            matched_client_txn: false,
        };
        assert!(!is_own(&sip));
    }
}
