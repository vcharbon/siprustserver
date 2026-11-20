use std::collections::BTreeMap;

use super::*;
use crate::state::RunState;
use pivot_schema::bundle::{Dir, Failure, IdentityBindings};
use pivot_schema::scoping::CheckClass;

struct FakeSut {
    active: usize,
    metrics: BTreeMap<String, String>,
}

impl Sut for FakeSut {
    fn active_calls(&self) -> usize {
        self.active
    }
    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        if name.starts_with("sip_") {
            Ok(self.metrics.get(name).cloned())
        } else {
            Err(format!("{name:?} is not a metric this system publishes"))
        }
    }
}

fn post(text: &str) -> Postconditions {
    serde_json::from_str(text).unwrap()
}

/// Settling waits on the flow and on the system's active calls, never on a
/// claim: a run whose call ended is settled before any record was written.
#[test]
fn settling_waits_on_the_flow_and_the_call_count_alone() {
    let mut sut = FakeSut { active: 1, metrics: BTreeMap::new() };
    assert!(!is_settled(true, &sut));
    assert!(!is_settled(false, &sut));
    assert_eq!(
        open_reasons(false, &sut),
        ["the flow has not completed", "the system reports 1 active call(s)"]
    );
    sut.active = 0;
    assert!(is_settled(true, &sut), "no record is waited for");
    assert!(open_reasons(true, &sut).is_empty());
}

/// The floor holds a non-2xx INVITE final a leg sent until its ACK or Timer
/// H (RFC 3261 §17.2.1); past Timer H the final is expired, not held, and
/// the reasons name only what is held.
#[test]
fn the_floor_holds_an_unacked_final_inside_timer_h_and_expires_it_past() {
    const INVITE: &str = "INVITE sip:b@127.0.0.1:5080 SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-2\r\n\
        From: <sip:a@example.test>;tag=a1\r\n\
        To: <sip:b@example.test>;tag=b1\r\n\
        Call-ID: call-a\r\n\
        CSeq: 2 INVITE\r\n\
        Content-Length: 0\r\n\r\n";
    const TERMINATED: &str = "SIP/2.0 487 Request Terminated\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-2\r\n\
        From: <sip:a@example.test>;tag=a1\r\n\
        To: <sip:b@example.test>;tag=b1\r\n\
        Call-ID: call-a\r\n\
        CSeq: 2 INVITE\r\n\
        Content-Length: 0\r\n\r\n";
    const ACK: &str = "ACK sip:b@127.0.0.1:5080 SIP/2.0\r\nMax-Forwards: 70\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-2\r\n\
        From: <sip:a@example.test>;tag=a1\r\n\
        To: <sip:b@example.test>;tag=b1\r\n\
        Call-ID: call-a\r\n\
        CSeq: 2 ACK\r\n\
        Content-Length: 0\r\n\r\n";
    let recording = Recording::new(std::sync::Arc::new(sip_message::CustomParser::built()));
    recording.declare("B");
    recording.push("B", Dir::In, 1_000_000, INVITE, None, None);
    recording.push("B", Dir::Out, 1_400_000, TERMINATED, None, None);
    let awaited = AwaitedAck {
        leg: "B".into(),
        owed: UnackedFinal { cseq: 2, status: 487, sent_at_us: 1_400_000 },
    };

    let held = floor(&recording, 1_400_000 + TIMER_H * 1000 - 1);
    assert_eq!(held, Floor { held: vec![awaited.clone()], ..Floor::default() });
    assert!(held.holds());
    assert_eq!(held.reasons(), ["leg B: 487 to INVITE CSeq 2 awaits its ACK"]);
    let expired = floor(&recording, 1_400_000 + TIMER_H * 1000);
    assert_eq!(expired, Floor { expired: vec![awaited], ..Floor::default() });
    assert!(!expired.holds(), "an expired transaction holds nothing open");
    assert!(expired.reasons().is_empty());

    recording.push("B", Dir::In, 1_600_000, ACK, None, None);
    assert_eq!(floor(&recording, 1_600_000), Floor::default(), "the ACK ended it");
}

/// The INVITE a leg sent holds the run open until its final (RFC 3261
/// §17.1.1.2): Timer B bounds it only while it is Calling, and past Timer
/// B a Calling INVITE is unanswered and holds nothing further; once a
/// provisional has arrived, only a final ends the wait.
#[test]
fn the_floor_holds_a_sent_invite_until_its_final_and_bounds_calling_by_timer_b() {
    fn response(status: &str) -> String {
        format!(
            "SIP/2.0 {status}\r\nContact: <sip:fixture@192.0.2.98>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:a@example.test>;tag=a1\r\n\
             To: <sip:b@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }
    const INVITE: &str = "INVITE sip:b@127.0.0.1:5080 SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:a@example.test>;tag=a1\r\n\
        To: <sip:b@example.test>\r\n\
        Call-ID: call-a\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";
    let timer_b_us = TIMER_B * 1000;
    let sent_at_us = 1_000_000;
    let calling = AwaitedFinal {
        leg: "A".into(),
        invite: PendingInvite { cseq: 1, sent_at_us, proceeding: false },
    };

    let recording = Recording::new(std::sync::Arc::new(sip_message::CustomParser::built()));
    recording.declare("A");
    recording.push("A", Dir::Out, sent_at_us, INVITE, None, None);
    // A retransmission is the same transaction: it moves no clock.
    recording.push_repeat("A", Dir::Out, sent_at_us + 500_000, INVITE, None, None);
    let held = floor(&recording, sent_at_us + timer_b_us - 1);
    assert_eq!(held, Floor { awaiting: vec![calling.clone()], ..Floor::default() });
    assert!(held.holds());
    assert_eq!(held.reasons(), ["leg A: INVITE CSeq 1 awaits its final"]);
    let timed_out = floor(&recording, sent_at_us + timer_b_us);
    assert_eq!(timed_out, Floor { unanswered: vec![calling], ..Floor::default() });
    assert!(!timed_out.holds(), "a Calling INVITE past Timer B holds nothing open");

    recording.push("A", Dir::In, sent_at_us + 200_000, response("100 Trying"), None, None);
    let proceeding = floor(&recording, sent_at_us + 10 * timer_b_us);
    let awaited = AwaitedFinal {
        leg: "A".into(),
        invite: PendingInvite { cseq: 1, sent_at_us, proceeding: true },
    };
    assert_eq!(proceeding, Floor { awaiting: vec![awaited], ..Floor::default() });
    assert_eq!(proceeding.reasons(), ["leg A: INVITE CSeq 1 awaits its final"]);

    let at = sent_at_us + 11 * timer_b_us;
    recording.push("A", Dir::In, at, response("486 Busy Here"), None, None);
    assert_eq!(floor(&recording, at), Floor::default(), "the final ended it");
}

/// The settle evaluation reads the system's observables alone: the
/// call-records claim, however it fails, is not its to answer.
#[test]
fn a_postcondition_finding_carries_the_class_of_the_check_that_found_it() {
    let expectation = post(
        r#"{"cdr":{"count":1,"checks":[{"field":"disposition","op":"eq","value":"ANSWERED"}]},
           "checks":[{"field":"sip_orphans","op":"eq","value":"0","class":"cdr-vocabulary"},
                     {"field":"sip_drops","op":"eq","value":"0"}]}"#,
    );
    let sut = FakeSut {
        active: 0,
        metrics: BTreeMap::from([
            ("sip_orphans".to_string(), "3".to_string()),
            ("sip_drops".to_string(), "1".to_string()),
        ]),
    };
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let findings = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
    let classes: Vec<Option<CheckClass>> = findings.iter().map(|f| f.class).collect();
    assert_eq!(classes, [Some(CheckClass::CdrVocabulary), None], "{findings:#?}");
    assert!(
        findings.iter().all(|f| matches!(f.failure, Failure::CheckFailed { .. })),
        "no record was read: {findings:#?}"
    );
}

#[test]
fn a_metric_the_system_does_not_publish_fails_rather_than_reading_as_zero() {
    let expectation = post(
        r#"{"cdr":{"absent":"n/a"},"checks":[{"field":"widgets_total","op":"eq","value":"0"}]}"#,
    );
    let sut = FakeSut { active: 0, metrics: BTreeMap::new() };
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let failures = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        matches!(&failures[0].failure, Failure::CheckFailed { observed, .. } if observed.starts_with("unreadable"))
    );
}
