//! The dispatch table: every event offered to a call's queue belongs to one
//! [`DispatchClass`], and its [`Row`] states everything the queue decides
//! about it. A new kind of event is one new class and one new row.
//!
//! Room: **B** bounded (no room at a full queue or at the global cap), **PB**
//! past bounds (waits past both, up to the call's overflow ceiling), **A**
//! always (waits past every bound; lost only behind the call's release).
//!
//! Permits: **NC** one of the new-call share, then one of the shared pool;
//! **S** one of the shared pool. A queue opens below: **H** the global cap
//! less the new-call headroom; **C** the global cap.
//!
//! | class | room | permits | opens a queue below | counts to cap | past lifetime cap | owed: no room / released | owed: capped |
//! |---|---|---|---|---|---|---|---|
//! | `InitialInvite` | B | NC | H | yes | no | 503 + Retry-After (no room only) | 503 + Retry-After |
//! | `EmergencyInvite` | B | S | C | yes | no | 503 + Retry-After (no room only) | 503 + Retry-After |
//! | `InDialogInvite` | B | S | C | yes | no | 500 + Retry-After (481 behind a terminated call's release) | 481 |
//! | `Ack` | B | S | C | yes | no | nothing | nothing |
//! | `OtherRequest` | B | S | C | yes | no | forget the transaction | BYE 200, other 481 |
//! | `StrayCancel` | B | S | C | yes | no | nothing | 481 stateless |
//! | `Response` | B | S | C | yes | yes | nothing | — |
//! | `TxnOutcome` | A | S | C | yes | yes | nothing | — |
//! | `Cancelled` | PB | S | C | yes | yes | nothing | — |
//! | `Timer` | A | S | C | no | yes | nothing | — |
//! | `Internal` | A | S | C | no | yes | nothing | — |
//!
//! A normal new call's turn holds its permits across the decision round
//! trip, so new calls hold at most their share of the shared pool and the
//! established calls keep the rest; an emergency new call draws the shared
//! pool alone. A bounded row opens a call's queue only below its threshold,
//! so a normal new INVITE leaves the last queues to the in-dialog requests of
//! calls taken over here and to emergency calls; the other rooms open a
//! queue past the cap (ADR-0037).
//!
//! A new INVITE, and every event after it, is never discarded behind its
//! call's release: it waits for the call's next queue ([`super::queue`]). A
//! new INVITE's row owes its answer for no room and on a capped call only;
//! its `ended` answer is never owed.
//!
//! `CallQuiesced` and an out-of-dialog OPTIONS are handled before any queue
//! and have no class. The per-interval message cap is a third bound, kept on
//! the call's turn (`router::process`), not here.

use sip_message::{Method, SipMessage};

use super::queue::Discard;
use crate::admission::Class;
use crate::metrics::RemovalClass;
use b2bua_sdk::event::CallEvent;

/// What one event is to its call's queue, derived once from the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatchClass {
    /// An INVITE with no To-tag: the start of a new normal call.
    InitialInvite,
    /// An INVITE with no To-tag carrying an emergency Resource-Priority
    /// (RFC 4412): the start of a new emergency call.
    EmergencyInvite,
    /// An INVITE in a dialog.
    InDialogInvite,
    /// An ACK, which draws no response.
    Ack,
    /// Any other request that is not a CANCEL.
    OtherRequest,
    /// A CANCEL that matched no transaction here: the layer hands it up
    /// unanswered.
    StrayCancel,
    /// A provisional, or a final no client transaction matched (a 2xx its
    /// UAS repeats until ACKed, RFC 3261 §13.3.1.4, or a stray).
    Response,
    /// A client transaction's outcome, which the layer delivers once: a final
    /// it matched to the transaction, or its `Timeout`.
    TxnOutcome,
    /// The layer answered a peer's CANCEL 200 and its INVITE 487; the caller
    /// will not send it again.
    Cancelled,
    /// A timer the call armed, fired once.
    Timer,
    /// An event the node raised itself: a reaper verdict, or the answer to a
    /// request the call sent off its turn.
    Internal,
}

impl DispatchClass {
    /// Every class, in table order.
    pub const ALL: [DispatchClass; 11] = [
        DispatchClass::InitialInvite,
        DispatchClass::EmergencyInvite,
        DispatchClass::InDialogInvite,
        DispatchClass::Ack,
        DispatchClass::OtherRequest,
        DispatchClass::StrayCancel,
        DispatchClass::Response,
        DispatchClass::TxnOutcome,
        DispatchClass::Cancelled,
        DispatchClass::Timer,
        DispatchClass::Internal,
    ];

    /// The class of `event`; `None` for a `CallQuiesced`, which no queue
    /// sees.
    pub fn of(event: &CallEvent) -> Option<Self> {
        Some(match event {
            CallEvent::Sip { message, matched_client_txn, .. } => match message.as_ref() {
                SipMessage::Request(req) => match req.method() {
                    Method::Invite => match crate::admission::class_of(req) {
                        Class::Normal => DispatchClass::InitialInvite,
                        Class::Emergency => DispatchClass::EmergencyInvite,
                        Class::InDialog => DispatchClass::InDialogInvite,
                    },
                    Method::Ack => DispatchClass::Ack,
                    Method::Cancel => DispatchClass::StrayCancel,
                    _ => DispatchClass::OtherRequest,
                },
                SipMessage::Response(resp) if *matched_client_txn && resp.status() >= 200 => {
                    DispatchClass::TxnOutcome
                }
                SipMessage::Response(_) => DispatchClass::Response,
            },
            CallEvent::Timeout { .. } => DispatchClass::TxnOutcome,
            CallEvent::Cancelled { .. } => DispatchClass::Cancelled,
            CallEvent::Timer { .. } => DispatchClass::Timer,
            CallEvent::InternalEvent { .. } => DispatchClass::Internal,
            CallEvent::CallQuiesced { .. } => return None,
        })
    }

    /// A new initial INVITE, normal or emergency: its admission is counted
    /// once as a new call, never as a dispatch drop.
    pub const fn is_new_call(self) -> bool {
        matches!(self, DispatchClass::InitialInvite | DispatchClass::EmergencyInvite)
    }

    /// The class's row of the table.
    pub const fn row(self) -> Row {
        use Owed::*;
        match self {
            // The 100 Trying stopped the caller's retransmissions: every
            // discard answers it, the global cap's included (ADR-0022). The
            // call it would start was never looked at, so the answer is the
            // capacity refusal, retryable. Behind a release it waits instead
            // of being discarded, so `ended` is never owed.
            DispatchClass::InitialInvite => Row {
                pool: PermitPool::NewCall,
                opens_queue_below: QueueThreshold::NewCallHeadroom,
                ..Row::bounded(NewCallRefused, NewCallRefused, NewCallRefused)
            },
            // Answered as a normal one, with the shared pool and the full cap
            // to draw on.
            DispatchClass::EmergencyInvite => {
                Row::bounded(NewCallRefused, NewCallRefused, NewCallRefused)
            }
            // A discard answers it too. Only a terminated or capped call is
            // known to be ending the dialog (RFC 3261 §12.2.2); elsewhere it
            // lives on — a shed takeover copy at its primary, an orphan
            // release that looked nothing up — and the retry may succeed.
            DispatchClass::InDialogInvite => Row::bounded(RetryLater, DialogGone, DialogGone),
            DispatchClass::Ack => Row::bounded(Nothing, Nothing, Nothing),
            // Its server transaction absorbs retransmissions while it waits:
            // forgotten, the retransmission is admitted afresh. A capped call
            // refuses every retransmission too, so it is answered where
            // refused.
            DispatchClass::OtherRequest => Row::bounded(Forget, Forget, CappedRefusal),
            DispatchClass::StrayCancel => Row::bounded(Nothing, Nothing, CappedRefusal),
            // The teardown that ends a capped call ACKs a 2xx crossing its
            // CANCEL, re-ACKs its repeats and hears the answers to its BYEs.
            // The room stays bounded: a peer repeating them fills the queue
            // and no more.
            DispatchClass::Response => {
                Row { past_lifetime_cap: true, ..Row::bounded(Nothing, Nothing, Nothing) }
            }
            // At most three per transaction the call sent (a `Timeout` may
            // precede the final a CANCEL provokes, RFC 3261 §9.1, and a 2xx
            // may follow a non-2xx), so no peer floods with them.
            DispatchClass::TxnOutcome => Row::own_work().counted(),
            // One per CANCEL the layer answered, bounded by its producer like
            // a transaction outcome. It keeps its room on a capped call:
            // without it the call never learns the caller cancelled, and its
            // teardown and CDR would contradict the 487 the caller heard.
            DispatchClass::Cancelled => Row {
                room: Room::PastBounds,
                past_lifetime_cap: true,
                ..Row::bounded(Nothing, Nothing, Nothing)
            },
            // One fire per arm, the next arm taking a turn of the call.
            DispatchClass::Timer => Row::own_work(),
            // A reaper verdict (one per sweep, per failed handler body, or
            // per call at its overflow ceiling or lifetime cap) or one answer
            // per request the call sent off its turn.
            DispatchClass::Internal => Row::own_work(),
        }
    }
}

/// How far past the queue's bounds an event may wait instead of being
/// discarded for want of room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Room {
    /// Discarded at a full queue, and for a call with no queue at the global
    /// cap.
    Bounded,
    /// Waits past a full queue and past the global cap, while the call's
    /// overflow holds fewer such events than its ceiling. A call flooded
    /// past that ceiling is torn down by the reaper.
    PastBounds,
    /// Waits past every bound, the overflow ceiling included.
    Always,
}

/// What the router owes the sender of an event discarded unrun. Each discard
/// owes exactly one of an answer, a forget or nothing. A forget is a no-op on
/// a transaction that has already sent something, so no answer and forget of
/// one request can race into the wrong order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owed {
    /// Nothing: no response is due, or nothing would send one again.
    Nothing,
    /// Forget the request's server transaction while it has sent nothing, so
    /// the peer's retransmission (RFC 3261 §17.1.2.2) is admitted afresh.
    Forget,
    /// The new-call 503 with a jittered Retry-After: a new call refused
    /// before any call state exists.
    NewCallRefused,
    /// 500 with a jittered Retry-After (RFC 3261 §14.2): the dialog may live
    /// on.
    RetryLater,
    /// 481: the dialog is ending (RFC 3261 §12.2.2).
    DialogGone,
    /// The refusal of a capped call, through the request's transaction: a BYE
    /// 200 (the dialog still exists while the cap's teardown ends it,
    /// RFC 3261 §15.1.2), a CANCEL 481 statelessly (§9.2), any other 481.
    CappedRefusal,
}

/// The permit pools a class's handler bodies draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermitPool {
    /// The pool every handler body shares (`event_dispatch_concurrency`).
    Shared,
    /// A permit of the new-call share (`new_call_permit_share_percent` of the
    /// shared pool), then one of the shared pool, both held for the body's
    /// whole run.
    NewCall,
}

/// How many live queues a bounded row's event may find and still open a
/// queue for its call; past-bounds and always-room events open one past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueThreshold {
    /// The global cap (`per_call_queue_cap`).
    FullCap,
    /// The global cap less the new-call headroom
    /// (`new_call_queue_headroom_percent` of it).
    NewCallHeadroom,
}

impl QueueThreshold {
    /// The threshold under the global cap `cap` and the new-call `headroom`
    /// kept below it.
    pub const fn of(self, cap: usize, headroom: usize) -> usize {
        match self {
            QueueThreshold::FullCap => cap,
            QueueThreshold::NewCallHeadroom => cap.saturating_sub(headroom),
        }
    }
}

/// One row of the dispatch table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub room: Room,
    /// Counted toward the call's lifetime cap; the node's own work is not.
    pub counted: bool,
    /// Keeps its room on a call past its lifetime cap instead of being
    /// refused there.
    pub past_lifetime_cap: bool,
    /// Owed when there is no room for it, or behind the release of a call
    /// that lives on elsewhere or was never looked up.
    pub unrun: Owed,
    /// Owed behind the release of a terminated call.
    pub ended: Owed,
    /// Owed when a capped call refuses it.
    pub capped: Owed,
    pub pool: PermitPool,
    pub opens_queue_below: QueueThreshold,
}

impl Row {
    /// A bounded row for what a peer sends, owing `unrun`, `ended` and
    /// `capped`.
    const fn bounded(unrun: Owed, ended: Owed, capped: Owed) -> Self {
        Row {
            room: Room::Bounded,
            counted: true,
            past_lifetime_cap: false,
            unrun,
            ended,
            capped,
            pool: PermitPool::Shared,
            opens_queue_below: QueueThreshold::FullCap,
        }
    }

    /// The node's own work: always room, uncounted, kept past the lifetime
    /// cap, owing nothing.
    const fn own_work() -> Self {
        Row {
            room: Room::Always,
            counted: false,
            past_lifetime_cap: true,
            ..Row::bounded(Owed::Nothing, Owed::Nothing, Owed::Nothing)
        }
    }

    /// This row, counted toward the lifetime cap.
    const fn counted(self) -> Self {
        Row { counted: true, ..self }
    }

    /// What a discard for `why` owes.
    pub fn owed(&self, why: Discard) -> Owed {
        match why {
            Discard::Capped => self.capped,
            Discard::Released(RemovalClass::Terminated) => self.ended,
            Discard::QueueFull | Discard::AtCap | Discard::Released(_) => self.unrun,
        }
    }

    /// Admitted on a call past its lifetime cap.
    pub fn admitted_when_capped(&self) -> bool {
        self.room == Room::Always || self.past_lifetime_cap
    }
}

#[cfg(test)]
mod tests {
    use call::TimerType;
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

    use super::*;

    fn sip(raw: &str, matched_client_txn: bool) -> CallEvent {
        CallEvent::Sip {
            message: Box::new(CustomParser::new().parse(raw.as_bytes()).unwrap()),
            src: "10.0.0.2:5060".parse().unwrap(),
            matched_client_txn,
        }
    }

    /// `status` to an INVITE, as the layer hands it up.
    fn invite_response(status: u16, matched_client_txn: bool) -> CallEvent {
        sip(
            &format!(
                "SIP/2.0 {status} X\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-out\r\n\
                 From: <sip:a@127.0.0.1>;tag=a\r\nTo: <sip:b@10.0.0.2>;tag=b\r\n\
                 Call-ID: outcome@unit\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
            ),
            matched_client_txn,
        )
    }

    /// A `method` request from a peer, in the dialog `a`/`b` when `to_tag`.
    fn request(method: &str, to_tag: bool) -> CallEvent {
        request_with(method, to_tag, "")
    }

    /// [`request`] carrying the header lines `extra`.
    fn request_with(method: &str, to_tag: bool, extra: &str) -> CallEvent {
        let to_tag = if to_tag { ";tag=b" } else { "" };
        sip(
            &format!(
                "{method} sip:b2bua@127.0.0.1 SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-peer\r\n\
                 From: <sip:a@10.0.0.1>;tag=a\r\nTo: <sip:b@127.0.0.1>{to_tag}\r\n\
                 Call-ID: peer@unit\r\nCSeq: 2 {method}\r\n{extra}Content-Length: 0\r\n\r\n"
            ),
            false,
        )
    }

    /// Every final the layer matched to a transaction is its outcome, the
    /// 2xx to an INVITE included; a provisional and an unmatched final (a
    /// repeat) are plain responses.
    #[test]
    fn every_matched_final_is_an_outcome_and_a_provisional_or_a_repeat_is_not() {
        for status in [200, 486, 503] {
            assert_eq!(
                DispatchClass::of(&invite_response(status, true)),
                Some(DispatchClass::TxnOutcome),
                "{status}"
            );
        }
        for (status, matched) in [(180, true), (200, false)] {
            assert_eq!(
                DispatchClass::of(&invite_response(status, matched)),
                Some(DispatchClass::Response)
            );
        }
    }

    /// A request's class is its method and whether it names a dialog.
    #[test]
    fn a_request_is_classed_by_its_method_and_dialog() {
        for (method, to_tag, class) in [
            ("INVITE", false, DispatchClass::InitialInvite),
            ("INVITE", true, DispatchClass::InDialogInvite),
            ("ACK", true, DispatchClass::Ack),
            ("CANCEL", false, DispatchClass::StrayCancel),
            ("BYE", true, DispatchClass::OtherRequest),
            ("INFO", true, DispatchClass::OtherRequest),
            ("OPTIONS", false, DispatchClass::OtherRequest),
        ] {
            assert_eq!(DispatchClass::of(&request(method, to_tag)), Some(class), "{method}");
        }
    }

    /// A new INVITE is an emergency call's by its Resource-Priority
    /// (RFC 4412); an INVITE in a dialog stays in-dialog whatever it carries.
    #[test]
    fn a_new_invite_with_an_emergency_priority_is_an_emergency_call() {
        for (rph, to_tag, class) in [
            ("esnet.0", false, DispatchClass::EmergencyInvite),
            ("dsn.flash, wps.0", false, DispatchClass::EmergencyInvite),
            ("dsn.flash", false, DispatchClass::InitialInvite),
            ("esnet.0", true, DispatchClass::InDialogInvite),
        ] {
            let invite = request_with("INVITE", to_tag, &format!("Resource-Priority: {rph}\r\n"));
            assert_eq!(DispatchClass::of(&invite), Some(class), "{rph} to_tag={to_tag}");
        }
    }

    /// A call's timer fires once, whatever its kind: none is dropped for want
    /// of room, and none counts toward the lifetime cap.
    #[test]
    fn every_timer_fire_is_the_nodes_own_work() {
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
            TimerType::ServiceHttpAnswer { correlation_id: "svc:1".into() },
            TimerType::ServiceAdmitAnswer { correlation_id: "svc:2".into(), change: 3 },
            TimerType::FailureAnswer { change: 4, unanswered: serde_json::json!({}) },
            TimerType::service(call::MachineId::new("svc"), "key"),
        ] {
            let fire = CallEvent::Timer {
                timer_type: timer_type.clone(),
                call_ref: "c".into(),
                leg_id: Some("b".into()),
                incarnation: None,
            };
            assert_eq!(DispatchClass::of(&fire), Some(DispatchClass::Timer), "{timer_type:?}");
        }
        let row = DispatchClass::Timer.row();
        assert_eq!((row.room, row.counted), (Room::Always, false));
    }

    /// The answer to every kind of request a call sends off its turn arrives
    /// once, as an internal event: none is dropped for want of room.
    #[test]
    fn every_answer_to_a_request_the_call_sent_is_the_nodes_own_work() {
        for topic in [
            "call-failure-result",
            "call-release-result",
            "refer-http-result",
            "service-http-result",
            crate::limiter::report::LimiterAdmitResult::TOPIC,
            crate::limiter::refresh_batch::RefreshAnswered::TOPIC,
            crate::reaper::REAPER_TOPIC,
        ] {
            let answer = CallEvent::InternalEvent {
                call_ref: "c".into(),
                topic: topic.into(),
                outcome: "any".into(),
                payload: serde_json::json!({}),
                body: Vec::new(),
                incarnation: None,
            };
            assert_eq!(DispatchClass::of(&answer), Some(DispatchClass::Internal), "{topic}");
        }
        let row = DispatchClass::Internal.row();
        assert_eq!((row.room, row.counted), (Room::Always, false));
    }

    /// Every row is the table's, column by column.
    #[test]
    fn every_row_is_the_tables() {
        use crate::dispatch::expected::expected;
        let whys = [
            Discard::QueueFull,
            Discard::AtCap,
            Discard::Capped,
            Discard::Released(RemovalClass::Terminated),
            Discard::Released(RemovalClass::SelfRelease),
            Discard::Released(RemovalClass::Orphan),
        ];
        for class in DispatchClass::ALL {
            let (row, e) = (class.row(), expected(class));
            assert_eq!(
                (row.room, row.counted, row.past_lifetime_cap),
                (e.room, e.counted, e.past_lifetime_cap),
                "{class:?}"
            );
            assert_eq!(
                (row.unrun, row.ended, row.capped),
                (e.unrun, e.ended, e.capped),
                "{class:?}"
            );
            assert_eq!((row.pool, row.opens_queue_below), (e.pool, e.threshold), "{class:?}");
            for why in whys {
                assert_eq!(row.owed(why), e.owed(why), "{class:?} {why:?}");
            }
        }
    }

    /// Only an INVITE discard is answered at every site; any other request is
    /// answered only where a capped call refuses it.
    #[test]
    fn only_an_invite_is_answered_where_there_is_no_room() {
        for class in DispatchClass::ALL {
            let row = class.row();
            let answered_unrun = !matches!(row.unrun, Owed::Nothing | Owed::Forget);
            let invite = matches!(
                class,
                DispatchClass::InitialInvite
                    | DispatchClass::EmergencyInvite
                    | DispatchClass::InDialogInvite
            );
            assert_eq!(answered_unrun, invite, "{class:?}");
        }
    }
}
