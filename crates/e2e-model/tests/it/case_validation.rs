//! A Test case validates against the shipped shape registry at load: every
//! problem is reported, each naming the shape, the anchor or the input field;
//! the committed `e2e/schemas/*.schema.json` match the model (regenerate with
//! `cargo run -p xtask -- e2e-schema`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use e2e_model::model::{schemas, validate_case, ModelError, TestCase};
use e2e_model::{ShapeDescriptor, ShapeRegistry};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn parse_case(json: &str) -> TestCase {
    serde_json::from_str(json).expect("test JSON parses")
}

fn problems(case: &TestCase, shapes: &ShapeRegistry) -> Vec<String> {
    match validate_case(case, shapes, &BTreeMap::new()) {
        Err(ModelError::Invalid(problems)) => problems,
        other => panic!("expected validation failure, got {other:?}"),
    }
}

#[test]
fn an_unpublished_anchor_is_incompatible_and_named_precisely() {
    let case = parse_case(
        r#"{
            "id": "bad-anchor",
            "compatibleShapes": ["basic-call"],
            "checks": [
                { "on": "bob1.prack", "checks": [ { "field": "rack", "op": "exists" } ] }
            ]
        }"#,
    );
    let problems = problems(&case, &ShapeRegistry::with_defaults());
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("incompatible with shape \"basic-call\"")
            && problems[0].contains("\"prack\"")
            && problems[0].contains("initialInvite"),
        "the message names the shape, the anchor, and what IS published: {}",
        problems[0]
    );
}

#[test]
fn every_validation_problem_is_reported() {
    let case = parse_case(
        r#"{
            "id": "many-problems",
            "compatibleShapes": ["no-such-shape"],
            "checkSets": ["no-such-set"],
            "checks": [
                { "on": "bob1.nonsenseAnchor", "checks": [ { "field": "from", "op": "exists" } ] },
                { "on": "justoneword", "checks": [ { "field": "from", "op": "regex" } ] },
                { "on": "bob1.bye", "checks": [ { "field": "to", "op": "absent", "value": "x" } ] }
            ]
        }"#,
    );
    let all = problems(&case, &ShapeRegistry::with_defaults()).join("\n");
    assert!(all.contains("unknown Callflow shape \"no-such-shape\""), "{all}");
    assert!(all.contains("unknown check set \"no-such-set\""), "{all}");
    assert!(all.contains("\"nonsenseAnchor\"") && all.contains("canonical"), "{all}");
    assert!(all.contains("\"justoneword\"") && all.contains("<agent>.<anchor>"), "{all}");
    assert!(all.contains("op Regex requires a value"), "{all}");
    assert!(all.contains("op Absent takes no value"), "{all}");
}

#[test]
fn a_missing_required_input_is_incompatible_and_an_extras_key_supplies_it() {
    let mut shapes = ShapeRegistry::empty();
    shapes
        .register(ShapeDescriptor::new("needs-target").required_input(&["from", "rerouteTarget"]));
    let missing = parse_case(
        r#"{
            "id": "missing-required",
            "compatibleShapes": ["needs-target"],
            "input": { "core": { "from": "sip:a@b" } }
        }"#,
    );
    let problems = problems(&missing, &shapes);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("required input field \"rerouteTarget\" is missing"),
        "{}",
        problems[0]
    );

    let supplied = parse_case(
        r#"{
            "id": "has-required",
            "compatibleShapes": ["needs-target"],
            "input": {
                "core": { "from": "sip:a@b" },
                "extras": { "rerouteTarget": "sip:bob2@host" }
            }
        }"#,
    );
    validate_case(&supplied, &shapes, &BTreeMap::new()).expect("extras satisfy required input");
}

#[test]
fn the_committed_schemas_are_current() {
    let dir = workspace_root().join("e2e/schemas");
    for (stem, schema) in schemas() {
        let path = dir.join(format!("{stem}.schema.json"));
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {} ({e}); run `cargo run -p xtask -- e2e-schema`", path.display())
        });
        let current = serde_json::to_string_pretty(&schema).unwrap();
        assert_eq!(
            committed.trim_end(),
            current,
            "{stem}.schema.json drifted from the model; run `cargo run -p xtask -- e2e-schema`"
        );
    }
}
