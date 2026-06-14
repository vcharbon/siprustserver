//! Per-shape **schema projection** (experimental): the static `schema_for!`
//! derive gives drift-proof *structure*; this module narrows the stringly-typed
//! vocabulary fields the derive cannot — `Check.field` and `CheckBlock.on` — into
//! enums/patterns drawn from the live shape registry, so the web Monaco editor
//! offers exactly the `<agent>.<anchor>` selectors and field accessors a chosen
//! shape supports.
//!
//! [`test_case_schema(None)`] is the base "lens" (universal `field` + op/value
//! help, any `on`); [`test_case_schema(Some(shape))`] additionally pins `on` to
//! that shape's `agents() × anchors()` product. [`compatible_shapes`] derives the
//! read-only "this case also fits…" list the editor shows — a case is compatible
//! with every registered shape that publishes the anchors (and carries the
//! agents) its checks bind, and is fed the required input.
//!
//! Nothing here is committed to disk: the schemas are computed live and served by
//! `e2e-web`, so they cannot drift from the registry. The eventual no-drift home
//! for the vocabulary is a first-class `Block` model (per the design); this is the
//! pragmatic projection that delivers the autocomplete now.

use std::collections::BTreeMap;

use schemars::schema_for;
use serde_json::{Value, json};

use crate::model::{CheckBlock, CheckSet, TestCase};
use crate::selector;
use crate::shape::{Anchor, CallflowShape};

/// The `<agent>.<anchor>` selectors a shape supports — the cartesian product of
/// its [`CallflowShape::agents`] roster and [`CallflowShape::anchors`], in
/// agent-major order (`alice.initialInvite`, `alice.answer`, …, `bob1.*`).
pub fn selectors_for(shape: &dyn CallflowShape) -> Vec<String> {
    let mut out = Vec::new();
    for agent in shape.agents() {
        for anchor in shape.anchors() {
            out.push(format!("{agent}.{}", anchor.as_str()));
        }
    }
    out
}

/// The `Check` op/value coherence rules as JSON-schema `allOf` branches: `eq` /
/// `regex` require a `value`; `exists` / `absent` forbid one. (The same rule the
/// loader enforces — surfaced to the editor as live diagnostics.)
fn op_value_rules() -> Value {
    json!([
        {
            "if": { "properties": { "op": { "enum": ["regex", "eq"] } }, "required": ["op"] },
            "then": { "required": ["value"] }
        },
        {
            "if": { "properties": { "op": { "enum": ["exists", "absent"] } }, "required": ["op"] },
            "then": { "not": { "required": ["value"] } }
        }
    ])
}

/// The authored-`TestCase` schema, optionally narrowed to one shape "lens".
///
/// - `None` — the base lens: universal `Check.field` enum+patterns and the
///   op/value rules; `CheckBlock.on` left as a free string (used for the local
///   `.vscode` fallback and the editor's "pick a shape" placeholder).
/// - `Some(shape)` — additionally pin `CheckBlock.on` to `selectors_for(shape)`,
///   so only that shape's `<agent>.<anchor>` selectors complete and validate.
pub fn test_case_schema(shape: Option<&dyn CallflowShape>) -> Value {
    let mut root = serde_json::to_value(schema_for!(TestCase))
        .expect("TestCase schema serialises to JSON");

    let Some(defs) = root.get_mut("$defs").and_then(Value::as_object_mut) else {
        return root;
    };

    // Check.field → enum + open-form patterns; Check → op/value coherence.
    if let Some(check) = defs.get_mut("Check").and_then(Value::as_object_mut) {
        if let Some(props) = check.get_mut("properties").and_then(Value::as_object_mut) {
            props.insert("field".into(), selector::field_schema());
        }
        check.insert("allOf".into(), op_value_rules());
    }

    // CheckBlock.on → the chosen shape's selector enum (when a lens is given);
    // Input.extras → the shape's typed params schema (when it declares one).
    if let Some(shape) = shape {
        let selectors = selectors_for(shape);
        if let Some(cb) = defs.get_mut("CheckBlock").and_then(Value::as_object_mut) {
            if let Some(props) = cb.get_mut("properties").and_then(Value::as_object_mut) {
                props.insert(
                    "on".into(),
                    json!({
                        "description": format!(
                            "<agent>.<anchor> selector for shape {:?}. The shape drives \
                             agents [{}] and publishes anchors [{}].",
                            shape.id(),
                            shape.agents().join(", "),
                            shape.anchors().iter().map(Anchor::as_str).collect::<Vec<_>>().join(", "),
                        ),
                        "type": "string",
                        "enum": selectors,
                    }),
                );
            }
        }
        splice_params(defs, shape);
    }

    root
}

/// Replace the open `Input.extras` map with the shape's typed params schema (its
/// own nested `$defs` are hoisted into the root so `$ref`s still resolve). A
/// shape with no params leaves `extras` an open object.
fn splice_params(defs: &mut serde_json::Map<String, Value>, shape: &dyn CallflowShape) {
    let Some(mut params) = shape.params_schema() else { return };
    if let Some(obj) = params.as_object_mut() {
        // Hoist the params schema's own $defs into the root $defs (same `#/$defs/`
        // ref path), and drop the standalone-document keys before inlining.
        if let Some(Value::Object(inner)) = obj.remove("$defs") {
            for (k, v) in inner {
                defs.insert(k, v);
            }
        }
        obj.remove("$schema");
        obj.remove("title");
    }
    if let Some(input) = defs.get_mut("Input").and_then(Value::as_object_mut) {
        if let Some(props) = input.get_mut("properties").and_then(Value::as_object_mut) {
            props.insert("extras".into(), params);
        }
    }
}

/// The registered shapes a case is compatible with: every shape that (i) is fed
/// the required input, (ii) publishes every anchor the case's checks bind, and
/// (iii) carries every agent they name (the last only when the shape declares a
/// roster). Returned sorted by id (the `BTreeMap` order). This is the derived,
/// read-only "compatibleShapes" the editor shows — slightly sharper than the
/// loader's anchor-only check thanks to the roster, never looser.
pub fn compatible_shapes(
    case: &TestCase,
    shapes: &BTreeMap<String, Box<dyn CallflowShape>>,
    check_sets: &BTreeMap<String, CheckSet>,
) -> Vec<String> {
    // Gather every block the case binds (inline + via referenced check sets).
    let mut blocks: Vec<&CheckBlock> = case.checks.iter().collect();
    for set_id in &case.check_sets {
        if let Some(set) = check_sets.get(set_id) {
            blocks.extend(set.blocks.iter());
        }
    }

    let mut out = Vec::new();
    for (id, shape) in shapes {
        let inputs_ok = shape.required_input().iter().all(|f| case.input.provides(f));
        let roster = shape.agents();
        let supported = blocks.iter().all(|b| match b.selector() {
            Some((agent, name)) => {
                let anchor_ok = Anchor::parse(name).is_some_and(|a| shape.anchors().contains(&a));
                let agent_ok = roster.is_empty() || roster.contains(&agent);
                anchor_ok && agent_ok
            }
            None => false,
        });
        if inputs_ok && supported {
            out.push(id.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::registry;

    #[test]
    fn selectors_are_agent_major_product() {
        let reg = registry();
        let basic = reg.get("basic-call").unwrap();
        let sels = selectors_for(basic.as_ref());
        assert_eq!(
            sels,
            vec![
                "alice.initialInvite",
                "alice.answer",
                "alice.ack",
                "alice.bye",
                "bob1.initialInvite",
                "bob1.answer",
                "bob1.ack",
                "bob1.bye",
            ]
        );
    }

    #[test]
    fn base_schema_has_field_enum_and_op_rules() {
        let schema = test_case_schema(None);
        let check = &schema["$defs"]["Check"];
        assert!(check["properties"]["field"]["anyOf"].is_array());
        assert!(check["allOf"].is_array());
        // No shape lens ⇒ `on` stays a free string (no enum).
        assert!(schema["$defs"]["CheckBlock"]["properties"]["on"].get("enum").is_none());
    }

    #[test]
    fn shape_lens_pins_on_enum() {
        let reg = registry();
        let prack = reg.get("rerouting-prack").unwrap();
        let schema = test_case_schema(Some(prack.as_ref()));
        let on = &schema["$defs"]["CheckBlock"]["properties"]["on"];
        let enums: Vec<&str> =
            on["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(enums.contains(&"bob2.prack"));
        assert!(enums.contains(&"alice.firstProvisional"));
        // basic-call has no prack ⇒ never offered under this shape either.
        assert!(enums.iter().all(|s| !s.starts_with("bob3.")));
    }

    #[test]
    fn shape_lens_splices_typed_params_into_extras() {
        let reg = registry();
        // rerouting declares typed params → extras becomes a typed object.
        let rerouting = reg.get("rerouting").unwrap();
        let schema = test_case_schema(Some(rerouting.as_ref()));
        let extras = &schema["$defs"]["Input"]["properties"]["extras"];
        assert!(extras["properties"]["rejectStatus"].is_object(), "rejectStatus suggested");
        assert!(extras["properties"]["rejectReason"].is_object(), "rejectReason suggested");
        assert_eq!(extras["additionalProperties"], json!(false), "typo'd keys rejected");

        // basic-call declares none → extras stays an open map.
        let basic = reg.get("basic-call").unwrap();
        let base = test_case_schema(Some(basic.as_ref()));
        assert_eq!(base["$defs"]["Input"]["properties"]["extras"]["additionalProperties"], json!(true));
    }

    #[test]
    fn compatible_shapes_widen_by_anchor_subset() {
        let reg = registry();
        let check_sets = BTreeMap::new();
        // An initial-call-only case: binds anchors every full-call shape carries.
        let case: TestCase = serde_json::from_value(json!({
            "id": "t",
            "compatibleShapes": ["basic-call"],
            "checks": [
                { "on": "bob1.initialInvite", "checks": [
                    { "field": "from.userInfo", "op": "exists" } ] }
            ]
        }))
        .unwrap();
        let compat = compatible_shapes(&case, &reg, &check_sets);
        // bob1.initialInvite is published by every full-call shape that carries a
        // bob1 — so the case widens well beyond basic-call.
        assert!(compat.contains(&"basic-call".to_string()));
        assert!(compat.contains(&"rerouting".to_string()));
        assert!(compat.contains(&"rerouting-prack".to_string()));
    }
}
