use super::*;
use crate::state::RunState;
use pivot_schema::bundle::{Dir, IdentityBindings};
use pivot_schema::scoping::CheckClass;

struct FakeSut {
    active: usize,
    cdrs: Vec<BTreeMap<String, String>>,
    metrics: BTreeMap<String, String>,
}

impl Sut for FakeSut {
    fn active_calls(&self) -> usize {
        self.active
    }
    fn cdr_records(&self) -> Vec<BTreeMap<String, String>> {
        self.cdrs.clone()
    }
    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        if name.starts_with("sip_") {
            Ok(self.metrics.get(name).cloned())
        } else {
            Err(format!("{name:?} is not a metric this system publishes"))
        }
    }
}

fn record(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn post(text: &str) -> Postconditions {
    serde_json::from_str(text).unwrap()
}

#[test]
fn settling_waits_on_the_flow_the_call_count_and_the_cdr_count() {
    let expectation = post(r#"{"cdr":{"count":1}}"#);
    let mut sut = FakeSut { active: 1, cdrs: vec![], metrics: BTreeMap::new() };
    assert!(!is_settled(true, &sut, Some(&expectation)));
    assert!(!is_settled(false, &sut, Some(&expectation)));
    sut.active = 0;
    assert!(!is_settled(true, &sut, Some(&expectation)), "the CDR has not been written");
    let reasons = open_reasons(false, &sut, Some(&expectation));
    assert_eq!(reasons, ["the flow has not completed", "0 of 1 CDR(s) written"]);
    sut.cdrs.push(record(&[("disposition", "ANSWERED")]));
    assert!(is_settled(true, &sut, Some(&expectation)));
    assert!(open_reasons(true, &sut, Some(&expectation)).is_empty());
}

/// The floor holds a non-2xx INVITE final a leg sent until its ACK or Timer
/// H (RFC 3261 §17.2.1); past Timer H the final is expired, not held, and
/// the reasons name only what is held.
#[test]
fn the_floor_holds_an_unacked_final_inside_timer_h_and_expires_it_past() {
    const INVITE: &str = "INVITE sip:b@127.0.0.1:5080 SIP/2.0\r\n\
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
    const ACK: &str = "ACK sip:b@127.0.0.1:5080 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-2\r\n\
        From: <sip:a@example.test>;tag=a1\r\n\
        To: <sip:b@example.test>;tag=b1\r\n\
        Call-ID: call-a\r\n\
        CSeq: 2 ACK\r\n\
        Content-Length: 0\r\n\r\n";
    let recording = Recording::new();
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
            "SIP/2.0 {status}\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:a@example.test>;tag=a1\r\n\
             To: <sip:b@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }
    const INVITE: &str = "INVITE sip:b@127.0.0.1:5080 SIP/2.0\r\n\
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

    let recording = Recording::new();
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

#[test]
fn a_reasoned_cdr_absence_waits_for_nothing_and_asserts_nothing() {
    let expectation = post(r#"{"cdr":{"absent":"capture-carries-no-cdr"}}"#);
    let sut = FakeSut { active: 0, cdrs: vec![], metrics: BTreeMap::new() };
    assert!(is_settled(true, &sut, Some(&expectation)));
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    assert!(evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings)).is_empty());
}

#[test]
fn the_cdr_count_is_exact_at_evaluation_even_though_settling_waits_for_at_least() {
    let expectation = post(r#"{"cdr":{"count":1}}"#);
    let sut = FakeSut {
        active: 0,
        cdrs: vec![record(&[("disposition", "ANSWERED")]), record(&[("disposition", "BUSY")])],
        metrics: BTreeMap::new(),
    };
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let failures = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
    assert_eq!(failures[0].class, None, "a record COUNT is not a vocabulary");
    let Failure::CdrMismatch { expected, observed } = &failures[0].failure else {
        panic!("a CDR mismatch: {failures:?}")
    };
    assert_eq!(expected, "1 record(s)");
    assert!(observed.starts_with("2 record(s):"), "{observed}");
    assert!(observed.contains("disposition=\"ANSWERED\""), "{observed}");
}

#[test]
fn a_cdr_check_asserts_over_the_record_set_and_says_so_when_none_matches() {
    let expectation = post(
        r#"{"cdr":{"count":2,"checks":[{"field":"disposition","op":"regex","value":"^(ANSWERED|CANCELLED)$"}]}}"#,
    );
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&state, &bindings);
    let matching = FakeSut {
        active: 0,
        cdrs: vec![record(&[("disposition", "ANSWERED")]), record(&[("disposition", "BUSY")])],
        metrics: BTreeMap::new(),
    };
    assert!(evaluate(&matching, Some(&expectation), &resolver).is_empty());
    let none = FakeSut {
        active: 0,
        cdrs: vec![record(&[("disposition", "BUSY")]), record(&[("disposition", "BUSY")])],
        metrics: BTreeMap::new(),
    };
    let failures = evaluate(&none, Some(&expectation), &resolver);
    assert_eq!(failures.len(), 1, "{failures:?}");
    // The failure carries what was actually billed, not only how many.
    let Failure::CdrMismatch { observed, .. } = &failures[0].failure else {
        panic!("a CDR mismatch")
    };
    assert!(observed.contains("disposition=\"BUSY\""), "{observed}");
}

#[test]
fn a_postcondition_finding_carries_the_class_of_the_check_that_found_it() {
    let expectation = post(
        r#"{"cdr":{"count":1,"checks":[
            {"field":"events","op":"regex","value":"InviteReceived","class":"cdr-vocabulary"},
            {"field":"disposition","op":"eq","value":"ANSWERED"}]},
           "checks":[{"field":"sip_orphans","op":"eq","value":"0","class":"cdr-vocabulary"}]}"#,
    );
    let sut = FakeSut {
        active: 0,
        cdrs: vec![record(&[("events", "invite_received"), ("disposition", "CANCELLED")])],
        metrics: BTreeMap::from([("sip_orphans".to_string(), "3".to_string())]),
    };
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let findings = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
    // Every check is EVALUATED; the class only says what the finding costs.
    let classes: Vec<Option<CheckClass>> = findings.iter().map(|f| f.class).collect();
    assert_eq!(
        classes,
        [Some(CheckClass::CdrVocabulary), None, Some(CheckClass::CdrVocabulary)],
        "{findings:#?}"
    );
}

#[test]
fn a_metric_the_system_does_not_publish_fails_rather_than_reading_as_zero() {
    let expectation = post(
        r#"{"cdr":{"absent":"n/a"},"checks":[{"field":"widgets_total","op":"eq","value":"0"}]}"#,
    );
    let sut = FakeSut { active: 0, cdrs: vec![], metrics: BTreeMap::new() };
    let state = RunState::new();
    let bindings = IdentityBindings::new();
    let failures = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        matches!(&failures[0].failure, Failure::CheckFailed { observed, .. } if observed.starts_with("unreadable"))
    );
}
