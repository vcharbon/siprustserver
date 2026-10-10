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
        assert_eq!((row.unrun, row.ended, row.capped), (e.unrun, e.ended, e.capped), "{class:?}");
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
