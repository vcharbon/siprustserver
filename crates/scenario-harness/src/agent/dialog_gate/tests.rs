//! A clean-end waiver covers the `dialog-ended` verdicts on a BYE recorded
//! from its start, one per dialog; every other verdict still gates.

use std::sync::Arc;
use std::time::Duration;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::surviving_verdicts;
use crate::agent::waiver::WaiverState;
use crate::dialogs::{DialogJudge, DialogJudging, DIALOG_ENDED};
use crate::WaiverScope;

fn message(text: String) -> SipMessage {
    CustomParser::new().parse(text.as_bytes()).expect("a well-formed test message")
}

/// A request of the caller `A1` on bob's dialog `B1` (none for the INVITE).
fn request(method: &str, cseq: u32) -> SipMessage {
    let to = if method == "INVITE" { String::new() } else { ";tag=B1".to_string() };
    message(format!(
        "{method} sip:bob@host SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
         From: <sip:a@host>;tag=A1\r\nTo: <sip:bob@host>{to}\r\nCall-ID: c1@host\r\n\
         CSeq: {cseq} {method}\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    ))
}

/// Bob's `status` to the request `method` numbered `cseq`.
fn response(status: u16, method: &str, cseq: u32) -> SipMessage {
    message(format!(
        "SIP/2.0 {status} Reason\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
         From: <sip:a@host>;tag=A1\r\nTo: <sip:bob@host>;tag=B1\r\nCall-ID: c1@host\r\n\
         CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    ))
}

#[tokio::test(start_paused = true)]
async fn a_clean_end_waiver_covers_one_bye_per_dialog_from_its_start() {
    let judge = Arc::new(DialogJudge::new("bob", Default::default(), DialogJudging::Harness));
    judge.sight(&request("INVITE", 1));
    judge.note_sent(&response(200, "INVITE", 1));
    judge.sight(&request("BYE", 2));
    judge.note_sent(&response(200, "BYE", 2));
    judge.sight(&request("BYE", 3));
    tokio::time::sleep(Duration::from_millis(1)).await;
    let scope = WaiverScope::rule(DIALOG_ENDED, "the clean end's BYE")
        .on_party("bob")
        .on_method("BYE")
        .from_now_once_per_dialog();
    tokio::time::sleep(Duration::from_millis(1)).await;
    judge.sight(&request("BYE", 4));
    judge.sight(&request("BYE", 5));
    judge.sight(&request("OPTIONS", 6));

    let (left, waived) = surviving_verdicts(&[judge], &[WaiverState::new(scope)]);
    let left: Vec<(String, u32)> =
        left.iter().map(|g| (g.verdict.method.clone(), g.verdict.cseq)).collect();
    let expected = [("BYE", 3), ("BYE", 5), ("OPTIONS", 6)].map(|(m, c)| (m.to_string(), c));
    assert_eq!(left, expected, "before the start, a second on the dialog, another method");
    assert_eq!(waived.get(DIALOG_ENDED), Some(&1));
}

/// A NOTIFY on bob's dialog stating its subscription `state`.
fn notify(cseq: u32, state: &str) -> SipMessage {
    message(format!(
        "NOTIFY sip:bob@host SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
         From: <sip:a@host>;tag=A1\r\nTo: <sip:bob@host>;tag=B1\r\nCall-ID: c1@host\r\n\
         CSeq: {cseq} NOTIFY\r\nEvent: refer\r\nSubscription-State: {state}\r\n\
         Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    ))
}

#[tokio::test(start_paused = true)]
async fn a_terminating_waiver_covers_only_the_notify_ending_its_subscription() {
    let judge = Arc::new(DialogJudge::new("bob", Default::default(), DialogJudging::Harness));
    judge.sight(&request("INVITE", 1));
    judge.note_sent(&response(200, "INVITE", 1));
    judge.sight(&request("BYE", 2));
    judge.note_sent(&response(200, "BYE", 2));
    let scope = WaiverScope::rule(DIALOG_ENDED, "the clean end's terminating NOTIFY")
        .on_party("bob")
        .on_method("NOTIFY")
        .terminating()
        .from_now_once_per_dialog();
    tokio::time::sleep(Duration::from_millis(1)).await;
    judge.sight(&notify(3, "active;expires=60"));
    judge.sight(&notify(4, "terminated;reason=noresource"));

    let (left, waived) = surviving_verdicts(&[judge], &[WaiverState::new(scope)]);
    let left: Vec<u32> = left.iter().map(|g| g.verdict.cseq).collect();
    assert_eq!(left, [3], "the active NOTIFY still gates");
    assert_eq!(waived.get(DIALOG_ENDED), Some(&1));
}
