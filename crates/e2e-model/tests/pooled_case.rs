//! The binding-pool half of `validate_case` (the parameters axis) + the
//! committed example pooled case: `e2e/cases/load-basic-pooled.json` must load
//! and validate — it is both documentation and the loadgen smoke fixture.

use std::collections::BTreeMap;
use std::path::PathBuf;

use e2e_model::model::{ModelError, TestCase, load_test_case, validate_case};
use e2e_model::shape::{Anchor, ShapeSpec};

/// A minimal load-time stand-in for the compiled `basic-call` shape (the full
/// registry lives in `e2e-core`; the model crate only needs the `ShapeSpec`
/// metadata slice).
struct StubShape {
    required: &'static [&'static str],
}

impl ShapeSpec for StubShape {
    fn anchors(&self) -> &[Anchor] {
        Anchor::ALL
    }
    fn required_input(&self) -> &[&str] {
        self.required
    }
}

fn shapes(required: &'static [&'static str]) -> BTreeMap<String, Box<StubShape>> {
    [("basic-call".to_string(), Box::new(StubShape { required }))].into()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn parse(json: &str) -> TestCase {
    serde_json::from_str(json).unwrap()
}

#[test]
fn the_committed_pooled_example_loads_and_validates() {
    let path = workspace_root().join("e2e/cases/load-basic-pooled.json");
    let case = load_test_case(&path).unwrap_or_else(|e| panic!("{e}"));
    let pool = case.bindings.as_ref().expect("the example carries a binding pool");
    assert_eq!(pool.entries.len(), 2);
    validate_case(&case, &shapes(&[]), &BTreeMap::new()).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn an_empty_binding_pool_fails_at_load() {
    let case = parse(
        r#"{ "id": "t", "compatibleShapes": ["basic-call"],
             "bindings": { "mode": "seq", "entries": [] } }"#,
    );
    let err = validate_case(&case, &shapes(&[]), &BTreeMap::new()).unwrap_err();
    assert!(err.to_string().contains("bindings.entries is empty"), "{err}");
}

#[test]
fn a_malformed_token_in_a_pool_entry_fails_at_load_with_its_location() {
    let case = parse(
        r#"{ "id": "t", "compatibleShapes": ["basic-call"],
             "input": { "core": { "from": "sip:${seq}@ok" } },
             "bindings": { "mode": "random", "entries": [
               { "core": { "from": "sip:ok@x" } },
               { "core": { "to": "sip:${bogus}@x" } }
             ] } }"#,
    );
    let err = validate_case(&case, &shapes(&[]), &BTreeMap::new()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("bindings.entries[1]"), "{msg}");
    assert!(msg.contains("${bogus}"), "{msg}");
    // Exactly the one problem: the base input's `${seq}` and entry 0 are fine.
    match err {
        ModelError::Invalid(problems) => assert_eq!(problems.len(), 1, "{problems:?}"),
        other => panic!("expected Invalid, got {other}"),
    }
}

#[test]
fn required_input_is_satisfied_by_every_entry_or_the_base() {
    // `from` comes from EVERY entry → compatible even though the base lacks it.
    let all_entries = parse(
        r#"{ "id": "t", "compatibleShapes": ["basic-call"],
             "bindings": { "mode": "seq", "entries": [
               { "core": { "from": "sip:a@x" } },
               { "core": { "from": "sip:b@x" } }
             ] } }"#,
    );
    validate_case(&all_entries, &shapes(&["from"]), &BTreeMap::new())
        .unwrap_or_else(|e| panic!("{e}"));

    // One entry misses `from` and the base does not provide it → incompatible
    // (that call would run without the required field).
    let gap = parse(
        r#"{ "id": "t", "compatibleShapes": ["basic-call"],
             "bindings": { "mode": "seq", "entries": [
               { "core": { "from": "sip:a@x" } },
               { "core": { "to": "sip:b@x" } }
             ] } }"#,
    );
    let err = validate_case(&gap, &shapes(&["from"]), &BTreeMap::new()).unwrap_err();
    assert!(err.to_string().contains("required"), "{err}");

    // The base providing it covers every call regardless of the entries.
    let base_covers = parse(
        r#"{ "id": "t", "compatibleShapes": ["basic-call"],
             "input": { "core": { "from": "sip:base@x" } },
             "bindings": { "mode": "seq", "entries": [
               { "core": { "to": "sip:b@x" } }
             ] } }"#,
    );
    validate_case(&base_covers, &shapes(&["from"]), &BTreeMap::new())
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn a_case_without_bindings_is_unchanged_single_input_behaviour() {
    let case = parse(
        r#"{ "id": "t", "compatibleShapes": ["basic-call"],
             "input": { "core": { "from": "sip:solo@x" } } }"#,
    );
    assert!(case.bindings.is_none());
    validate_case(&case, &shapes(&["from"]), &BTreeMap::new()).unwrap_or_else(|e| panic!("{e}"));
}
