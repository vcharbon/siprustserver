use super::*;
use crate::state::RunState;
use pivot_schema::bundle::{ClockMode, IdentityBindings, RunConfig, VerdictStatus};
use pivot_schema::scoping::CheckClass;

fn record(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn expected(text: &str) -> CdrCheck {
    serde_json::from_str(text).unwrap()
}

/// `expected` as a run on `lane` states it, for a document whose content
/// came from `origin`.
fn stated(text: &str, lane: &str, origin: Option<&str>) -> RecordsClaim {
    let config = RunConfig::new(lane, ClockMode::Virtual, "h:1");
    let run = RunState::new();
    let bindings = IdentityBindings::new();
    let resolver = Resolver::new(&run, &bindings);
    super::state(&expected(text), &resolver, &Scope::new(&config, origin))
}

fn verdict() -> RunVerdict {
    RunVerdict::ok("c", "lane")
}

#[test]
fn the_record_count_is_exact() {
    let claim = stated(r#"{"count":1}"#, "lane", None);
    let two = [record(&[("disposition", "ANSWERED")]), record(&[("disposition", "BUSY")])];
    let mut judged = verdict();
    assert!(!judge(&claim, &two, &mut judged));
    assert_eq!(judged.status, VerdictStatus::Failed);
    let [Failure::CdrMismatch { expected, observed }] = &judged.failures[..] else {
        panic!("one record mismatch: {:?}", judged.failures)
    };
    assert_eq!(expected, "1 record(s)");
    assert!(observed.starts_with("2 record(s):"), "{observed}");
    assert!(observed.contains("disposition=\"ANSWERED\""), "{observed}");

    let mut none = verdict();
    assert!(!judge(&claim, &[], &mut none));
    assert!(
        matches!(&none.failures[..], [Failure::CdrMismatch { observed, .. }] if observed == "no record was written")
    );

    let mut one = verdict();
    assert!(judge(&claim, &two[..1], &mut one));
    assert!(one.passed(), "{one:?}");
}

#[test]
fn a_check_asserts_over_the_record_set_and_says_so_when_none_matches() {
    let claim = stated(
        r#"{"count":2,"checks":[{"field":"disposition","op":"regex","value":"^(ANSWERED|CANCELLED)$"}]}"#,
        "lane",
        None,
    );
    let mut matching = verdict();
    let some = [record(&[("disposition", "ANSWERED")]), record(&[("disposition", "BUSY")])];
    assert!(judge(&claim, &some, &mut matching));
    assert!(matching.passed());

    let mut unmatched = verdict();
    let busy = [record(&[("disposition", "BUSY")]), record(&[("disposition", "BUSY")])];
    assert!(!judge(&claim, &busy, &mut unmatched));
    let [Failure::CdrMismatch { expected, observed }] = &unmatched.failures[..] else {
        panic!("one record mismatch: {:?}", unmatched.failures)
    };
    assert!(expected.starts_with("some record where disposition"), "{expected}");
    assert!(observed.contains("disposition=\"BUSY\""), "{observed}");
}

/// A classified check gates on its origin lane and is informative elsewhere:
/// the run states which, and the judgement records it there.
#[test]
fn a_classified_check_is_informative_off_its_origin_lane() {
    let text = r#"{"count":1,"checks":[
        {"field":"events","op":"regex","value":"InviteReceived","class":"cdr-vocabulary"},
        {"field":"disposition","op":"eq","value":"ANSWERED"}]}"#;
    let records = [record(&[("events", "invite_received"), ("disposition", "CANCELLED")])];

    let foreign = stated(text, "here", Some("origin"));
    assert_eq!(foreign.checks.iter().map(|c| c.gates).collect::<Vec<_>>(), [false, true]);
    let mut judged = verdict();
    assert!(!judge(&foreign, &records, &mut judged));
    assert_eq!(judged.failures.len(), 1, "only the unclassified check gates: {judged:?}");
    assert_eq!(judged.informative.len(), 1);
    assert_eq!(judged.informative[0].class, CheckClass::CdrVocabulary);

    let home = stated(text, "origin", Some("origin"));
    let mut judged = verdict();
    assert!(!judge(&home, &records, &mut judged));
    assert_eq!(judged.failures.len(), 2, "both gate on the origin lane: {judged:?}");
    assert!(judged.informative.is_empty());
}

/// A value carrying no accessor is stated as written, a regex included; one
/// whose accessor the run cannot resolve stays in the claim, saying why.
#[test]
fn a_check_value_is_stated_resolved_and_an_unresolved_one_says_why() {
    let claim = stated(
        r#"{"count":1,"checks":[
            {"field":"a","op":"regex","value":"^x+$"},
            {"field":"b","op":"exists"},
            {"field":"c","op":"eq","value":"${step:s9.cseq}"}]}"#,
        "lane",
        None,
    );
    let values: Vec<(&str, Option<&str>, bool)> = claim
        .checks
        .iter()
        .map(|c| (c.check.field.as_str(), c.check.value.as_deref(), c.unresolved.is_some()))
        .collect();
    assert_eq!(
        values,
        [("a", Some("^x+$"), false), ("b", None, false), ("c", Some("${step:s9.cseq}"), true)]
    );
}

/// A check whose value does not resolve fails the claim on the record line
/// whatever the records hold, and keeps its class: it gates on its origin lane
/// and is informative off it.
#[test]
fn an_unresolved_check_fails_the_claim_by_its_class() {
    let text = r#"{"count":1,"checks":[
        {"field":"call_id","op":"eq","value":"${step:s9.cseq}","class":"cdr-vocabulary"}]}"#;
    let records = [record(&[("call_id", "${step:s9.cseq}")])];

    let mut home = verdict();
    assert!(!judge(&stated(text, "origin", Some("origin")), &records, &mut home));
    let [Failure::CdrMismatch { expected, observed }] = &home.failures[..] else {
        panic!("one record mismatch: {:?}", home.failures)
    };
    assert!(expected.starts_with("some record where call_id"), "{expected}");
    assert!(
        observed.starts_with("the check's value does not resolve against the run"),
        "{observed}"
    );
    assert!(home.informative.is_empty());

    let mut foreign = verdict();
    assert!(judge(&stated(text, "here", Some("origin")), &records, &mut foreign));
    assert!(foreign.passed(), "{foreign:?}");
    assert_eq!(foreign.informative.len(), 1);
    assert_eq!(foreign.informative[0].class, CheckClass::CdrVocabulary);
}

/// A stated value is the run's resolved one: judging reads it as written, so a
/// value that resolved to text holding `${…}` matches a record holding it.
#[test]
fn a_stated_value_is_compared_as_written_never_resolved_again() {
    let literal = "x${leg:A.remote-tag}";
    let stated_check = |op: CheckOp, value: &str| StatedCheck {
        check: Check { field: "note".into(), op, value: Some(value.into()), class: None },
        gates: true,
        unresolved: None,
    };
    let claim = RecordsClaim {
        count: 1,
        checks: vec![
            stated_check(CheckOp::Eq, literal),
            stated_check(CheckOp::Regex, &regex::escape(literal)),
        ],
    };
    let mut judged = verdict();
    assert!(judge(&claim, &[record(&[("note", literal)])], &mut judged), "{judged:?}");
    assert!(judged.passed(), "{judged:?}");
}

#[test]
fn a_stated_claim_round_trips() {
    let claim =
        stated(r#"{"count":1,"checks":[{"field":"a","op":"eq","value":"v"}]}"#, "lane", None);
    let text = serde_json::to_string(&claim).unwrap();
    assert_eq!(text, r#"{"count":1,"checks":[{"field":"a","op":"eq","value":"v","gates":true}]}"#);
    assert_eq!(serde_json::from_str::<RecordsClaim>(&text).unwrap(), claim);
}
