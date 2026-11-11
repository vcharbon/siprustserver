use super::*;
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
fn stated(text: &str, lane: &str, origin: Option<&str>) -> (RecordsClaim, Vec<Failure>) {
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
    let (claim, failures) = stated(r#"{"count":1}"#, "lane", None);
    assert!(failures.is_empty());
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
    let (claim, _) = stated(
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

    let (foreign, _) = stated(text, "here", Some("origin"));
    assert_eq!(foreign.checks.iter().map(|c| c.gates).collect::<Vec<_>>(), [false, true]);
    let mut judged = verdict();
    assert!(!judge(&foreign, &records, &mut judged));
    assert_eq!(judged.failures.len(), 1, "only the unclassified check gates: {judged:?}");
    assert_eq!(judged.informative.len(), 1);
    assert_eq!(judged.informative[0].class, CheckClass::CdrVocabulary);

    let (home, _) = stated(text, "origin", Some("origin"));
    let mut judged = verdict();
    assert!(!judge(&home, &records, &mut judged));
    assert_eq!(judged.failures.len(), 2, "both gate on the origin lane: {judged:?}");
    assert!(judged.informative.is_empty());
}

/// A value carrying no accessor is stated as written, a regex included; one
/// whose accessor the run cannot resolve is a failure and leaves the claim.
#[test]
fn a_check_value_is_stated_resolved_and_an_unresolved_one_is_a_failure() {
    let (claim, failures) = stated(
        r#"{"count":1,"checks":[
            {"field":"a","op":"regex","value":"^x+$"},
            {"field":"b","op":"exists"},
            {"field":"c","op":"eq","value":"${step:s9.cseq}"}]}"#,
        "lane",
        None,
    );
    let values: Vec<(&str, Option<&str>)> =
        claim.checks.iter().map(|c| (c.check.field.as_str(), c.check.value.as_deref())).collect();
    assert_eq!(values, [("a", Some("^x+$")), ("b", None)]);
    assert!(
        matches!(&failures[..], [Failure::AccessorUnresolved { site, .. }] if site == "cdr check \"c\""),
        "{failures:?}"
    );
}

#[test]
fn a_stated_claim_round_trips() {
    let (claim, _) =
        stated(r#"{"count":1,"checks":[{"field":"a","op":"eq","value":"v"}]}"#, "lane", None);
    let text = serde_json::to_string(&claim).unwrap();
    assert_eq!(text, r#"{"count":1,"checks":[{"field":"a","op":"eq","value":"v","gates":true}]}"#);
    assert_eq!(serde_json::from_str::<RecordsClaim>(&text).unwrap(), claim);
}
