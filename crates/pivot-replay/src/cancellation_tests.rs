use super::*;
use pivot_schema::bundle::Dir;

/// A call from the caller uac1 (leg A, on ep0) to the callee uas1 (leg B, on
/// `callee_endpoint`): `flow` its steps, each `(id, leg, op, msg, flags)`,
/// `extra` the document's further top-level fields.
fn call_document(
    callee_endpoint: &str,
    flow: &[(&str, &str, &str, &str, &str)],
    extra: &str,
) -> PivotV3 {
    let d = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;
    let steps: Vec<String> = flow
        .iter()
        .map(|(id, leg, op, msg, flags)| {
            format!(r#"{{ "id": "{id}", "leg": "{leg}", "op": "{op}", "msg": {msg}, "delay": {d}{flags} }}"#)
        })
        .collect();
    let text = format!(
        r#"{{
          "pivot_version": 3,
          "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                    "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
          "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
          "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
             {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }} ],
          "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }},
                         {{ "id": "ep1", "observed": "127.0.0.1:5062", "side": "peer", "binding": "dedicated" }} ],
          "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                      {{ "id": "uas1", "kind": "uas", "endpoint": "{callee_endpoint}" }} ],
          "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                    {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
          "flow": [ {} ],
          {extra}
          "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
        }}"#,
        steps.join(",\n")
    );
    PivotV3::from_json(&text).expect("the document parses")
}

const IN_DIALOG: &str = r#", "in_dialog": true"#;

/// A call whose callee re-INVITEs (s15) and takes a 481 to it (s22) it never
/// ACKs, which the document states as the callee's violation.
fn reinvite_reject_unacked() -> PivotV3 {
    call_document(
        "ep0",
        &[
            ("s1", "A", "send", r#"{ "method": "INVITE" }"#, ""),
            ("s3", "B", "expect", r#"{ "method": "INVITE" }"#, ""),
            ("s11", "B", "send", r#"{ "status": 200 }"#, ""),
            ("s12", "A", "expect", r#"{ "status": 200 }"#, ""),
            ("s15", "B", "send", r#"{ "method": "INVITE" }"#, IN_DIALOG),
            ("s19", "B", "send", r#"{ "method": "BYE" }"#, IN_DIALOG),
            ("s22", "B", "expect", r#"{ "status": 481, "cseq-method": "INVITE" }"#, IN_DIALOG),
        ],
        r#""rfc_violations": [ { "emitter": "uas1", "rule": "unacked-invite-non-2xx-final", "step": "s22" } ],"#,
    )
}

/// A call whose caller withholds the ACK (s8) of the callee's 2xx, which the
/// document states as a `suppress-auto` deviation.
fn withheld_ack() -> PivotV3 {
    call_document(
        "ep1",
        &[
            ("s1", "A", "send", r#"{ "method": "INVITE" }"#, ""),
            ("s3", "B", "expect", r#"{ "method": "INVITE" }"#, ""),
            ("s6", "B", "send", r#"{ "status": 200 }"#, ""),
            ("s7", "A", "expect", r#"{ "status": 200 }"#, ""),
            (
                "s8",
                "A",
                "send",
                r#"{ "method": "ACK" }"#,
                r#", "auto": true, "confirms_dialog": true, "in_dialog": true"#,
            ),
        ],
        r#""deviations": [ { "id": "d1", "kind": "suppress-auto", "leg": "A", "step": "s8" } ],"#,
    )
}

fn datagram(first: &str, cseq: &str, branch: &str) -> Vec<u8> {
    format!(
        "{first}\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
         Call-ID: c1\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn recorded(seq: u64, dir: Dir, step: Option<&str>, wire: Vec<u8>) -> RecordedMessage {
    RecordedMessage::new(
        seq,
        dir,
        seq * 1_000,
        step.map(str::to_string),
        wire,
        &sip_message::CustomParser::new(),
    )
}

/// A document stating the callee's withheld ACK on the reject it took: the
/// run's reject — unclaimed by any step — is cancelled through the re-INVITE
/// that opened its transaction, and a reject on another transaction is not.
#[test]
fn a_stated_violation_cancels_the_finding_on_its_own_transaction_only() {
    let doc = reinvite_reject_unacked();
    let reinvite = datagram("INVITE sip:s@h SIP/2.0", "1 INVITE", "z9hG4bK-re");
    let reject = datagram("SIP/2.0 487 Request Terminated", "1 INVITE", "z9hG4bK-re");
    let legs = BTreeMap::from([(
        "B".to_string(),
        vec![
            recorded(1, Dir::Out, Some("s15"), reinvite),
            recorded(2, Dir::In, None, reject.clone()),
        ],
    )]);
    let statements = Statements::of(&doc, legs);
    let rule = "unacked-invite-non-2xx-final";
    assert_eq!(statements.actor_named("ep0", &reject).as_deref(), Some("uas1"));
    assert_eq!(
        statements.cancelling(rule, "uas1", &reject).as_deref(),
        Some("rfc_violations: unacked-invite-non-2xx-final at s22 by uas1")
    );
    let other = datagram("SIP/2.0 487 Request Terminated", "1 INVITE", "z9hG4bK-other");
    assert_eq!(statements.cancelling(rule, "uas1", &other), None, "another transaction");
    assert_eq!(statements.cancelling("no-200-after-cancel", "uas1", &reject), None);
    assert_eq!(
        statements.cancelling(rule, "uac1", &reject),
        None,
        "the caller sharing the callee's endpoint is another party"
    );
}

/// An authored scenario's `suppress-auto` on the caller's ACK states the
/// withheld ACK of the 2xx that ACK would have answered.
#[test]
fn a_withheld_ack_deviation_cancels_the_missing_acks_finding() {
    let doc = withheld_ack();
    let ok = datagram("SIP/2.0 200 OK", "1 INVITE", "z9hG4bK-a");
    let legs = BTreeMap::from([(
        "A".to_string(),
        vec![
            recorded(
                1,
                Dir::Out,
                Some("s1"),
                datagram("INVITE sip:b@h SIP/2.0", "1 INVITE", "z9hG4bK-a"),
            ),
            recorded(2, Dir::In, Some("s7"), ok.clone()),
        ],
    )]);
    let statements = Statements::of(&doc, legs);
    assert_eq!(statements.actor_named("ep0", &ok).as_deref(), Some("uac1"));
    assert_eq!(
        statements.cancelling("no-ack-to-dialog-creating-2xx", "uac1", &ok).as_deref(),
        Some("deviation d1 (suppress-auto) withholds s8")
    );
    assert_eq!(statements.cancelling("no-ack-to-dialog-creating-2xx", "uas1", &ok), None);
}

/// A two-leg document: the caller A answers the callee's re-INVITE on leg
/// A (s2, an `expect` of the re-INVITE at s1), and the callee B takes the
/// platform's relayed answer on leg B (s4, the re-INVITE B sent at s3).
/// `violations` is its `rfc_violations` block.
fn relaying_document(violations: &str) -> PivotV3 {
    let d = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;
    let text = format!(
        r#"{{
          "pivot_version": 3,
          "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                    "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
          "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
          "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
             {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }} ],
          "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
          "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                      {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
          "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                    {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
          "flow": [
            {{ "id": "s1", "leg": "A", "op": "expect", "msg": {{ "method": "INVITE" }}, "delay": {d} }},
            {{ "id": "s2", "leg": "A", "op": "send", "msg": {{ "status": 200 }}, "delay": {d} }},
            {{ "id": "s3", "leg": "B", "op": "send", "msg": {{ "method": "INVITE" }}, "delay": {d} }},
            {{ "id": "s4", "leg": "B", "op": "expect", "msg": {{ "status": 200 }}, "delay": {d} }}
          ],
          "rfc_violations": [{violations}],
          "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
        }}"#
    );
    PivotV3::from_json(&text).expect("the document parses")
}

const RULE: &str = "payload-type-mapping-stable";
const PARTY: &str = r#"{"rule":"payload-type-mapping-stable","step":"s2","emitter":"uac1"}"#;
const RELAYED: &str =
    r#"{"rule":"payload-type-mapping-stable","step":"s4","emitter":"sut","relays":"s2"}"#;

/// The run's recording: the caller's answer on leg A, and the platform's
/// relayed copy the callee takes on leg B — `relayed` is that copy, the
/// datagram the SUT's finding is charged on.
fn relaying_run(doc: &PivotV3) -> (Statements, Vec<u8>) {
    let reinvite_b = datagram("INVITE sip:a@h SIP/2.0", "7 INVITE", "z9hG4bK-b");
    let relayed = datagram("SIP/2.0 200 OK", "7 INVITE", "z9hG4bK-b");
    let legs = BTreeMap::from([
        (
            "A".to_string(),
            vec![
                recorded(
                    1,
                    Dir::In,
                    Some("s1"),
                    datagram("INVITE sip:a@h SIP/2.0", "3 INVITE", "z9hG4bK-a"),
                ),
                recorded(
                    2,
                    Dir::Out,
                    Some("s2"),
                    datagram("SIP/2.0 200 OK", "3 INVITE", "z9hG4bK-a"),
                ),
            ],
        ),
        (
            "B".to_string(),
            vec![
                recorded(3, Dir::Out, Some("s3"), reinvite_b),
                recorded(4, Dir::In, Some("s4"), relayed.clone()),
            ],
        ),
    ]);
    (Statements::of(doc, legs), relayed)
}

/// The document states the caller committing the violation AND the
/// platform relaying it: the SUT's relayed copy is cancelled on its own
/// transaction, named as caused by the caller, and nothing else is.
#[test]
fn a_relayed_violation_stated_with_its_origin_cancels_the_systems_copy() {
    let doc = relaying_document(&format!("{PARTY},{RELAYED}"));
    let (statements, relayed) = relaying_run(&doc);
    let cancelled = statements.relayed(RULE, &relayed).expect("cancelled");
    assert_eq!(
        cancelled.label,
        "relayed: payload-type-mapping-stable originated by uac1 at s2; the reference relays it too"
    );
    assert_eq!(cancelled.caused_by, "uac1");
    let other = datagram("SIP/2.0 200 OK", "8 INVITE", "z9hG4bK-other");
    assert!(statements.relayed(RULE, &other).is_none(), "another transaction");
    assert!(statements.relayed("final-2xx-answers-the-offer", &relayed).is_none());
}

/// The platform's relaying stated alone — its origin unstated — cancels
/// nothing: half the claim gates like no claim.
#[test]
fn a_relayed_violation_without_its_origin_cancels_nothing() {
    let doc = relaying_document(RELAYED);
    let (statements, relayed) = relaying_run(&doc);
    assert!(statements.relayed(RULE, &relayed).is_none());
    let elsewhere = relaying_document(&format!(
        r#"{{"rule":"payload-type-mapping-stable","step":"s1","emitter":"uac1"}},{RELAYED}"#
    ));
    let (statements, relayed) = relaying_run(&elsewhere);
    assert!(statements.relayed(RULE, &relayed).is_none(), "the origin is another step");
}

/// The caller's violation stated alone — the platform's relaying unstated —
/// cancels the caller's own finding and never the SUT's.
#[test]
fn an_origin_without_the_relaying_statement_leaves_the_systems_copy_gating() {
    let doc = relaying_document(PARTY);
    let (statements, relayed) = relaying_run(&doc);
    assert!(statements.relayed(RULE, &relayed).is_none());
    let answer = datagram("SIP/2.0 200 OK", "3 INVITE", "z9hG4bK-a");
    assert!(statements.cancelling(RULE, "uac1", &answer).is_some());
}

/// A captured document flagged as cut from a capture the violation census
/// never covered says it never read its trace's violations; one that does
/// not carry the flag reads them, stated or not.
#[test]
fn a_document_cut_where_the_census_never_reached_is_unread() {
    let flagged = relaying_document("").with_flag(
        UNCOVERED_FLAG,
        "the census report covers no capture 'c': nothing states what its scripted parties broke",
    );
    assert_eq!(Statements::of(&flagged, BTreeMap::new()).unread().as_deref(), Some(UNREAD));
    let other = relaying_document("").with_flag("detector-roster", "relay18x");
    assert_eq!(Statements::of(&other, BTreeMap::new()).unread(), None);
    assert_eq!(Statements::of(&relaying_document(""), BTreeMap::new()).unread(), None);
}

trait WithFlag {
    fn with_flag(self, kind: &str, detail: &str) -> Self;
}

impl WithFlag for PivotV3 {
    fn with_flag(mut self, kind: &str, detail: &str) -> Self {
        self.case
            .annotations
            .get_or_insert_with(Default::default)
            .flags
            .push(pivot_schema::case::Flag { kind: kind.into(), detail: detail.into() });
        self
    }
}

/// A document that states nothing cancels nothing.
#[test]
fn nothing_stated_cancels_nothing() {
    let reject = datagram("SIP/2.0 487 Request Terminated", "1 INVITE", "z9hG4bK-re");
    assert_eq!(
        Statements::default().cancelling("unacked-invite-non-2xx-final", "uas1", &reject),
        None
    );
}
