use super::*;
use crate::IdentityBindings;
use pivot_schema::bundle::ClockMode;
use pivot_schema::scoping::CheckClass;

/// A two-leg document whose callee leg claims by the `by` given, stating
/// the origin lane its content came from where it names one.
fn plan_of(claim: &str, origin_lane: Option<&str>) -> Plan {
    let origin_lane =
        origin_lane.map(|lane| format!(r#", "origin_lane": "{lane}""#)).unwrap_or_default();
    let text = format!(
        r#"{{
              "pivot_version": 3,
              "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                        "origin": "authored", "lanes": {{ "upstream-fake": "ok" }}{origin_lane} }},
              "identities": [
                {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }},
                {{ "name": "called-0-0", "kind": "site", "forms": ["e164"] }} ],
              "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
                 {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "called-0-0" }} }} ] }} ],
              "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
              "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                          {{ "id": "uas1", "kind": "uas", "endpoint": "ep0", "claim": {{ "by": "{claim}" }} }} ],
              "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                        {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
              "flow": [ {{ "id": "s1", "leg": "A", "op": "send", "msg": {{ "method": "INVITE" }},
                          "delay": {{ "ms": 0, "from": "trigger", "compressible": true, "timer_linked": false }} }} ],
              "postconditions": {{ "cdr": {{ "absent": "unit test" }} }},
              "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
            }}"#
    );
    Plan::compile(pivot_schema::PivotV3::from_json(&text).expect("the fixture parses"))
        .expect("the fixture compiles")
}

fn plan(claim: &str) -> Plan {
    plan_of(claim, None)
}

fn config(bindings: IdentityBindings) -> RunConfig {
    RunConfig::new("upstream-demo", ClockMode::Virtual, "127.0.0.1:5080").with_identities(bindings)
}

fn mismatch() -> Failure {
    Failure::CdrMismatch {
        expected: "some record where events matches Bye".into(),
        observed: "1 record(s): {events=\"bye\"}".into(),
    }
}

/// Compile once, run MANY: two instances of one plan must not share the
/// identity nonce their dialogs are minted from.
#[test]
fn two_instances_of_one_plan_mint_disjoint_nonces() {
    let plan = plan("arrival-order");
    let bound = || config(IdentityBindings::new().bind("called-0-0", "e164", "0900004"));
    let first = Instance::new(&plan, bound());
    let second = Instance::new(&plan, bound());
    assert_ne!(first.nonce(), second.nonce());
    assert!(!first.nonce().is_empty());
}

/// A lane that seeds the nonce knows the ids its dials carry before the
/// run starts.
#[test]
fn an_instance_takes_the_nonce_its_lane_seeds() {
    let plan = plan("arrival-order");
    let bound = config(IdentityBindings::new().bind("called-0-0", "e164", "0900004"));
    let instance = Instance::new(&plan, bound.with_identity_nonce("lane-seed-1"));
    assert_eq!(instance.nonce(), "lane-seed-1");
}

#[test]
fn a_ruri_pos_claim_the_lane_left_unbound_refuses_before_the_run_dials() {
    let plan = plan("ruri-pos");
    // The lane bound nothing: the claim's number set would be empty, so the
    // run would dial and then report an unclaimed arrival.
    let instance = Instance::new(&plan, config(IdentityBindings::new()));
    assert!(!instance.verdict().passed());
    let failure = &instance.verdict().failures[0];
    assert!(
        matches!(failure, Failure::IdentityUnbound { identity, detail, .. }
            if identity == "called-0-0" && detail.contains("e164")),
        "{failure:?}"
    );

    // Bound in the form the identity declares: nothing to refuse.
    let bound =
        Instance::new(&plan, config(IdentityBindings::new().bind("called-0-0", "e164", "0900004")));
    assert!(bound.verdict().passed(), "{:?}", bound.verdict().failures);
}

#[test]
fn a_per_call_directive_that_reaches_no_dial_refuses_before_the_run_dials() {
    let plan = plan("arrival-order");
    let bound = || config(IdentityBindings::new().bind("called-0-0", "e164", "0900004"));

    // The document's own call: the directive lands on its opening INVITE.
    let placed = Instance::new(&plan, bound().with_call_header("c1", "X-Api-Call", "{}"));
    assert!(placed.verdict().passed(), "{:#?}", placed.verdict().failures);
    assert_eq!(plan.dial_of_call("c1"), Some("s1"));
    assert_eq!(plan.call_dialled_by("s1"), Some("c1"));

    // A call id nothing declares would stamp its header on nothing at all.
    let stray = Instance::new(&plan, bound().with_call_header("c9", "X-Api-Call", "{}"));
    let failure = &stray.verdict().failures[0];
    assert!(
        matches!(failure, Failure::CallDirectiveUnplaced { call, detail }
            if call == "c9" && detail.contains("no calls[] entry")),
        "{failure:?}"
    );
}

#[test]
fn a_classified_finding_from_another_lane_is_recorded_and_does_not_fail_the_run() {
    let plan = plan_of("arrival-order", Some("origin-platform"));
    let mut instance = Instance::new(&plan, config(IdentityBindings::new()));
    assert!(
        !instance.record(Finding::new(Some(CheckClass::CdrVocabulary), mismatch())),
        "a foreign lane's vocabulary does not gate"
    );
    assert!(instance.verdict().passed(), "{:#?}", instance.verdict().failures);
    assert_eq!(instance.verdict().informative.len(), 1);
    assert_eq!(instance.verdict().informative[0].class, CheckClass::CdrVocabulary);
    assert_eq!(instance.verdict().informative[0].finding, mismatch(), "evaluated, not skipped");

    // An unclassified finding is the protocol's own and gates everywhere.
    assert!(instance.record(Finding::gating(mismatch())));
    assert!(!instance.verdict().passed());
    assert_eq!(instance.verdict().failures.len(), 1);
}

#[test]
fn a_classified_finding_gates_on_its_own_lane_and_wherever_the_lane_states_so() {
    // Replayed at home, the origin platform's vocabulary is the system's own.
    let home = plan_of("arrival-order", Some("upstream-demo"));
    let mut instance = Instance::new(&home, config(IdentityBindings::new()));
    assert!(instance.record(Finding::new(Some(CheckClass::CdrVocabulary), mismatch())));
    assert!(!instance.verdict().passed());
    assert!(instance.verdict().informative.is_empty());

    // A foreign lane that shares the vocabulary states `gating` outright.
    let foreign = plan_of("arrival-order", Some("origin-platform"));
    let shared = config(IdentityBindings::new())
        .with_check_scoping(CheckClass::CdrVocabulary, CheckDisposition::Gating);
    let mut instance = Instance::new(&foreign, shared);
    assert!(instance.record(Finding::new(Some(CheckClass::CdrVocabulary), mismatch())));
    assert!(!instance.verdict().passed());

    // And the other direction: a lane replaying its OWN document under a
    // foreign header profile states `informative`.
    let borrowed = config(IdentityBindings::new())
        .with_check_scoping(CheckClass::OriginPlatformHeader, CheckDisposition::Informative);
    let mut instance = Instance::new(&home, borrowed);
    assert!(!instance.record(Finding::new(Some(CheckClass::OriginPlatformHeader), mismatch())));
    assert!(instance.verdict().passed(), "{:#?}", instance.verdict().failures);
    assert_eq!(instance.verdict().informative.len(), 1);
}

#[test]
fn a_document_naming_no_origin_lane_downgrades_nothing() {
    let plan = plan("arrival-order");
    let mut instance = Instance::new(&plan, config(IdentityBindings::new()));
    assert!(instance.record(Finding::new(Some(CheckClass::CdrVocabulary), mismatch())));
    assert!(!instance.verdict().passed());
    assert!(instance.verdict().informative.is_empty());
}

#[test]
fn a_claim_that_needs_no_number_is_not_refused_for_want_of_one() {
    // `arrival-order` tells INVITEs apart by nothing, so an unbound identity
    // costs it nothing and must not fail the run.
    let plan = plan("arrival-order");
    let instance = Instance::new(&plan, config(IdentityBindings::new()));
    assert!(instance.verdict().passed(), "{:?}", instance.verdict().failures);
}
