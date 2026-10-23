//! Strategies for the call's records: its attempts, timers, CDR events,
//! decision log, termination record and message ring.

use super::*;

pub(super) fn arb_attempt() -> impl Strategy<Value = Attempt> {
    (
        0usize..4,
        prop_oneof![
            Just(AttemptCause::Route),
            Just(AttemptCause::Failover),
            Just(AttemptCause::Reroute),
            Just(AttemptCause::Transfer),
        ],
        any::<u32>(),
        proptest::collection::vec("b-[0-9]{1,2}", 0..3),
        proptest::option::of(1i64..90_000),
        proptest::option::of(prop_oneof![
            Just(AttemptOutcome::Answered),
            Just(AttemptOutcome::Failed),
            Just(AttemptOutcome::Superseded),
            Just(AttemptOutcome::CallEnded),
            Just(AttemptOutcome::Refused),
        ]),
    )
        .prop_map(|(id, cause, decision_ordinal, legs, no_answer_ms, outcome)| Attempt {
            id: AttemptId(id),
            cause,
            decision_ordinal,
            legs,
            no_answer: Deadline::of_millis(no_answer_ms),
            outcome,
        })
}

pub(super) fn arb_ring() -> impl Strategy<Value = Ring> {
    prop_oneof![Just(Ring::Silent), Just(Ring::Rang), Just(Ring::Presumed)]
}

fn arb_message_direction() -> impl Strategy<Value = MessageDirection> {
    prop_oneof![
        Just(MessageDirection::Received),
        Just(MessageDirection::Relayed),
        Just(MessageDirection::Authored),
    ]
}

fn arb_message_entry() -> impl Strategy<Value = MessageEntry> {
    (
        any::<u32>(),
        any::<i64>(),
        arb_message_direction(),
        "(INVITE|ACK|BYE|PRACK|OPTIONS)",
        any::<u32>(),
        proptest::option::of(100u16..700),
        proptest::option::of(arb_tag()),
        any::<u32>(),
        proptest::collection::vec(("(Allow|Accept|Privacy)", "[a-zA-Z0-9, ]{0,24}"), 0..3),
        any::<u32>(),
    )
        .prop_map(
            |(
                seq,
                at_ms,
                direction,
                method,
                cseq,
                code,
                to_tag,
                decision_ordinal,
                headers,
                turn,
            )| {
                MessageEntry {
                    seq,
                    at_ms,
                    direction,
                    method,
                    cseq,
                    code,
                    to_tag,
                    decision_ordinal,
                    headers,
                    turn,
                }
            },
        )
}

fn arb_decision_kind() -> impl Strategy<Value = DecisionKind> {
    prop_oneof![
        Just(DecisionKind::Route),
        Just(DecisionKind::Reject),
        Just(DecisionKind::Redirect),
        Just(DecisionKind::Relay),
        Just(DecisionKind::FailoverRoute),
        Just(DecisionKind::FailoverReject),
        Just(DecisionKind::FailoverRedirect),
        Just(DecisionKind::FailoverTerminate),
        Just(DecisionKind::Release),
        Just(DecisionKind::ReleaseRoute),
        Just(DecisionKind::TransferAllow),
        Just(DecisionKind::TransferReject),
        Just(DecisionKind::InfoRelay),
        Just(DecisionKind::InfoAcknowledge),
    ]
}

/// Varied decision logs: empty (no decision yet) through a few marks, each
/// numbered by its position, with and without a label.
pub(super) fn arb_decision_log() -> impl Strategy<Value = Vec<DecisionMark>> {
    proptest::collection::vec(
        (
            any::<i64>(),
            arb_decision_kind(),
            proptest::option::of(arb_tag()),
            proptest::option::of("[ -~]{0,24}"),
        ),
        0..4,
    )
    .prop_map(|marks| {
        marks
            .into_iter()
            .enumerate()
            .map(|(i, (at_ms, kind, leg_id, label))| DecisionMark {
                ordinal: i as u32 + 1,
                at_ms,
                kind,
                leg_id,
                label,
            })
            .collect()
    })
}

fn arb_termination_cause() -> impl Strategy<Value = TerminationCause> {
    let timeout = prop_oneof![
        Just(TimeoutKind::Setup),
        Just(TimeoutKind::NoAnswer),
        Just(TimeoutKind::Ack),
        Just(TimeoutKind::Keepalive),
        Just(TimeoutKind::Transaction),
    ];
    prop_oneof![
        Just(TerminationCause::RemoteBye),
        Just(TerminationCause::RemoteCancel),
        Just(TerminationCause::RemoteFinal),
        Just(TerminationCause::DecisionReject),
        Just(TerminationCause::DecisionRelease),
        Just(TerminationCause::MaxDuration),
        timeout.prop_map(TerminationCause::Timeout),
        Just(TerminationCause::Admission),
        Just(TerminationCause::MessageCap),
        Just(TerminationCause::Supervisor),
        Just(TerminationCause::OperatorRelease),
    ]
}

/// Varied termination records: none (a live call) through every cause, with
/// and without a leg, cut or not yet.
pub(super) fn arb_termination() -> impl Strategy<Value = Option<Termination>> {
    proptest::option::of(
        (any::<i64>(), arb_termination_cause(), proptest::option::of(arb_tag()), any::<u32>())
            .prop_map(|(at_ms, cause, by_leg, last_seq)| Termination {
                at_ms,
                cause,
                by_leg,
                last_seq,
            }),
    )
}

/// Varied message rings: empty (the ring off) through a few entries with an
/// eviction count.
pub(super) fn arb_message_ring() -> impl Strategy<Value = MessageRing> {
    (proptest::collection::vec(arb_message_entry(), 0..4), 0u32..5)
        .prop_map(|(entries, dropped)| MessageRing { entries, dropped })
}

pub(super) fn arb_timer() -> impl Strategy<Value = TimerEntry> {
    let scope = prop_oneof![
        Just(TimerScope::Call),
        (0usize..4).prop_map(|id| TimerScope::Attempt(AttemptId(id))),
        arb_tag().prop_map(TimerScope::Leg),
    ];
    (arb_timer_type(), scope, any::<u64>(), any::<i64>()).prop_map(|(timer_type, scope, g, at)| {
        TimerEntry { key: TimerKey { timer_type, scope }, generation: g, fire_at: at }
    })
}

pub(super) fn arb_cdr() -> impl Strategy<Value = CdrEvent> {
    (
        arb_cdr_type(),
        any::<i64>(),
        arb_tag(),
        proptest::option::of(100i64..700),
        proptest::option::of("[a-z ]{0,20}"),
        any::<u32>(),
    )
        .prop_map(|(event_type, timestamp, leg_id, status_code, reason, decision_ordinal)| {
            CdrEvent { event_type, timestamp, leg_id, status_code, reason, decision_ordinal }
        })
}
