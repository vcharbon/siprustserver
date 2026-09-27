//! How the per-call dispatcher treats an event: how far past its bounds the
//! event may wait (the one-shot events nothing sends again, each bounded by
//! what produces it), whether it keeps its room on a call past its lifetime
//! cap, and whether it counts toward that cap (everything but the node's own
//! work).

use sip_message::SipMessage;

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
    /// The room `event` needs.
    ///
    /// - A `Cancelled`, which a peer's CANCEL produces (the layer already
    ///   answered 200 + 487, and the caller will not send it again), waits
    ///   past a full queue and the cap, up to the call's overflow ceiling. A
    ///   call flooded past it is torn down by the reaper.
    /// - What the node's own work produces waits past every bound: a reaper
    ///   verdict (paced by the sweep), a call's timer fire (one per arm, and
    ///   the next arm takes a turn of the call), and a client transaction's
    ///   outcome (at most three per transaction the call sent, see
    ///   [`is_transaction_outcome`]). None of them counts toward the overflow
    ///   ceiling, which detects a peer flooding the call. The lifetime cap
    ///   counts the outcomes, and admits them past it.
    pub(super) fn of(event: &CallEvent) -> Self {
        if crate::reaper::is_reaper_event(event)
            || matches!(event, CallEvent::Timer { .. })
            || is_transaction_outcome(event)
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

/// A response keeps its ordinary room on a call past its lifetime cap: the
/// teardown that ends the call ACKs a 2xx that crosses its CANCEL, re-ACKs
/// its repeats and hears the answers to its BYEs. A request, the start of
/// new work, is refused there.
pub(super) fn keeps_room_past_lifetime_cap(event: &CallEvent) -> bool {
    matches!(event, CallEvent::Sip { message, .. } if matches!(message.as_ref(), SipMessage::Response(_)))
}

/// A client transaction's outcome, each of which the layer delivers once:
/// its `Timeout`, or a final it matched to the transaction. The layer forgets
/// a transaction on its 2xx or non-INVITE final and hands their repeats up
/// unmatched; it ACKs a non-2xx INVITE final and absorbs its repeats, and
/// only a 2xx may still follow one. An INVITE that gave up is held for the
/// final its CANCEL provokes (RFC 3261 §9.1), so a `Timeout` may precede
/// both. A provisional (lossy by design) and an unmatched response (a 2xx
/// its UAS repeats until ACKed, §13.3.1.4, or a stray) are no outcome.
fn is_transaction_outcome(event: &CallEvent) -> bool {
    match event {
        CallEvent::Timeout { .. } => true,
        CallEvent::Sip { message, matched_client_txn: true, .. } => match message.as_ref() {
            SipMessage::Response(r) => r.status() >= 200,
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

    use super::{is_own, is_transaction_outcome, Room};
    use crate::event::CallEvent;

    /// `status` to an INVITE, as the layer hands it up.
    fn invite_response(status: u16, matched_client_txn: bool) -> CallEvent {
        let raw = format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-out\r\n\
             From: <sip:a@127.0.0.1>;tag=a\r\nTo: <sip:b@10.0.0.2>;tag=b\r\n\
             Call-ID: outcome@unit\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        CallEvent::Sip {
            message: Box::new(CustomParser::new().parse(raw.as_bytes()).unwrap()),
            src: "10.0.0.2:5060".parse().unwrap(),
            matched_client_txn,
        }
    }

    /// Every final the layer matched to a transaction is delivered once, the
    /// 2xx to an INVITE included (the layer forgets the transaction on it and
    /// hands its repeats up unmatched). A provisional and an unmatched final
    /// are no outcome.
    #[test]
    fn every_matched_final_is_an_outcome_and_a_provisional_or_a_repeat_is_not() {
        for status in [200, 486, 503] {
            assert!(is_transaction_outcome(&invite_response(status, true)), "{status}");
        }
        assert!(!is_transaction_outcome(&invite_response(180, true)));
        assert!(!is_transaction_outcome(&invite_response(200, false)));
    }

    /// A call's timer fires once, whatever its kind: none is dropped for want
    /// of room.
    #[test]
    fn every_timer_fire_waits_past_every_bound() {
        let ack = call::Obligation::AckOf2xx { leg: "a".into(), dialog_tag: "t".into(), cseq: 1 };
        for timer_type in [
            TimerType::NoAnswer,
            TimerType::SetupTimeout,
            TimerType::GlobalDuration,
            TimerType::LimiterRefresh,
            TimerType::Keepalive,
            TimerType::KeepaliveTimeout,
            TimerType::Rung { obligation: ack.clone() },
            TimerType::RepeatGiveUp { obligation: ack },
            TimerType::TerminatingTimeout,
            TimerType::ReferSubscriptionExpiry,
            TimerType::ReferReinviteAnswer,
            TimerType::ReferOverallSafety,
            TimerType::service(call::MachineId::new("svc"), "key"),
        ] {
            let fire = CallEvent::Timer {
                timer_type: timer_type.clone(),
                call_ref: "c".into(),
                leg_id: Some("b".into()),
            };
            assert!(matches!(Room::of(&fire), Room::PastAllBounds), "{timer_type:?}");
        }
    }

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
