use std::sync::Arc;

use scenario_harness::actor::scenarios::BasicCall;

use super::*;

/// A source built from runtime data, as a document directory would be: ids
/// and legs arrive as owned strings and are interned.
/// One authored shape: its id and its legs (role, R-URI prefixes).
type Doc = (String, Vec<(String, Vec<String>)>);

struct Documents(Vec<Doc>);

impl ShapeSource for Documents {
    fn origin(&self) -> String {
        "documents".to_string()
    }
    fn shapes(&self) -> Result<Vec<ShapeDescriptor>, String> {
        Ok(self
            .0
            .iter()
            .map(|(id, legs)| {
                let legs: Vec<LegSpec> = legs
                    .iter()
                    .map(|(role, prefixes)| {
                        let prefixes: Vec<&str> = prefixes.iter().map(String::as_str).collect();
                        LegSpec { role: intern(role), ruri_prefixes: intern_prefixes(&prefixes) }
                    })
                    .collect();
                ShapeDescriptor::new(intern(id))
                    .legs(intern_legs(&legs))
                    .load_shared(Arc::new(BasicCall))
            })
            .collect())
    }
}

struct Failing;

impl ShapeSource for Failing {
    fn origin(&self) -> String {
        "broken-dir".to_string()
    }
    fn shapes(&self) -> Result<Vec<ShapeDescriptor>, String> {
        Err("shape.json: no id".to_string())
    }
}

fn doc(id: &str, legs: &[(&str, &[&str])]) -> Doc {
    let legs = legs
        .iter()
        .map(|(role, p)| (role.to_string(), p.iter().map(|s| s.to_string()).collect()))
        .collect();
    (id.to_string(), legs)
}

#[test]
fn a_runtime_source_registers_beside_the_compiled_shapes() {
    let docs = Documents(vec![doc("doc_reroute", &[("bob", &["100"]), ("bob2", &["200"])])]);
    let mut reg = ShapeRegistry::with_defaults();
    reg.register_source(&docs).expect("a fresh id registers");
    let d = reg.get("doc_reroute").expect("registered");
    assert_eq!(d.callee_legs().len(), 2);
    assert_eq!(d.callee_legs()[1].ruri_prefixes, &["200"]);
    assert!(reg.get("basic_call").is_some(), "the compiled shapes stay");
}

#[test]
fn a_duplicate_id_names_its_source_and_registers_nothing() {
    let docs = Documents(vec![doc("fresh_one", &[]), doc("basic_call", &[])]);
    let mut reg = ShapeRegistry::with_defaults();
    let err = reg.register_source(&docs).map(|_| ()).unwrap_err();
    assert!(err.contains("documents") && err.contains("basic_call"), "{err}");
    assert!(reg.get("fresh_one").is_none(), "a refused source registers nothing");

    let twice = Documents(vec![doc("twin", &[]), doc("twin", &[])]);
    let err = ShapeRegistry::empty().register_source(&twice).map(|_| ()).unwrap_err();
    assert!(err.contains("twin"), "{err}");
}

#[test]
fn a_failing_source_is_an_error_naming_it() {
    let err = ShapeRegistry::from_sources(&[&Failing]).map(|_| ()).unwrap_err();
    assert!(err.contains("broken-dir") && err.contains("no id"), "{err}");
}

#[test]
fn interning_allocates_each_value_once() {
    let a = intern(&String::from("same_id"));
    let b = intern(&String::from("same_id"));
    assert!(std::ptr::eq(a, b));
    let legs = [LegSpec { role: intern("bob"), ruri_prefixes: intern_prefixes(&["9"]) }];
    let copy = [LegSpec { role: intern("bob"), ruri_prefixes: intern_prefixes(&["9"]) }];
    assert!(std::ptr::eq(intern_legs(&legs), intern_legs(&copy)));
}
