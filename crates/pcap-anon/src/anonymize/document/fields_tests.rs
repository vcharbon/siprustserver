//! The flows document carries every field upstream adds: each text field of
//! its schema is classified here as rewritten, stripped or vocabulary, so a
//! field the anonymiser has not considered fails the guard instead of
//! passing through in clear.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::model::Flows;

/// How the anonymiser treats a text field of the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Treatment {
    /// Mapped by [`super::rewrite`].
    Rewritten,
    /// Cleared by `sip_pcap::enrich::strip` or the header-failure clear, and
    /// re-derived from the anonymised bytes where it is derived.
    Stripped,
    /// Protocol vocabulary, a refusal reason's key or a tool's own text:
    /// names no one.
    Vocabulary,
    /// Protocol text kept by where it sits (a reason phrase), as on the wire.
    ProtocolText,
}

use Treatment::{ProtocolText, Rewritten, Stripped, Vocabulary};

/// Every text field of the document, by its path (`[]` an array item,
/// `{key}` a map key, `{value}` a map value), and how it is treated.
const FIELDS: &[(&str, Treatment)] = &[
    ("emit_headers[]", Vocabulary),
    ("rewritten", Vocabulary),
    ("flow_stats.header_failed_by_reason{key}", Vocabulary),
    ("flow_stats.parse_failed_by_reason{key}", Vocabulary),
    ("flow_stats.header_failures[].call_id", Stripped),
    ("flow_stats.header_failures[].header", Stripped),
    ("flow_stats.header_failures[].reason", Stripped),
    ("groups[].evidence[].as_socket", Rewritten),
    ("groups[].evidence[].peer_socket", Rewritten),
    ("groups[].evidence[].shared_host", Rewritten),
    ("groups[].evidence[].token", Rewritten),
    ("groups[].evidence[].header", Vocabulary),
    ("groups[].evidence[].param", Vocabulary),
    ("groups[].evidence[].prefix", Vocabulary),
    ("groups[].methods{key}", Stripped),
    ("groups[].methods{value}.content_types[]", Stripped),
    ("legs[].call_id", Rewritten),
    ("legs[].hops[].a", Rewritten),
    ("legs[].hops[].b", Rewritten),
    ("legs[].invite.from_uri", Rewritten),
    ("legs[].invite.ruri", Rewritten),
    ("legs[].invite.to_uri", Rewritten),
    ("legs[].terminated_by", Rewritten),
    ("legs[].tokens[]", Rewritten),
    ("legs[].msgs[].raw", Rewritten),
    ("legs[].msgs[].raw_b64", Rewritten),
    ("legs[].msgs[].head", Rewritten),
    ("legs[].msgs[].body_b64", Rewritten),
    ("legs[].msgs[].src", Rewritten),
    ("legs[].msgs[].dst", Rewritten),
    ("legs[].msgs[].summary.method", Rewritten),
    ("legs[].msgs[].summary.uri", Rewritten),
    ("legs[].msgs[].summary.from.uri", Rewritten),
    ("legs[].msgs[].summary.from.tag", Rewritten),
    ("legs[].msgs[].summary.to.uri", Rewritten),
    ("legs[].msgs[].summary.to.tag", Rewritten),
    ("legs[].msgs[].summary.cseq.method", Vocabulary),
    ("legs[].msgs[].summary.reason", ProtocolText),
    ("legs[].msgs[].body.content_type", Stripped),
    ("legs[].msgs[].body.parts[].content_id", Stripped),
    ("legs[].msgs[].body.parts[].content_type", Stripped),
    ("legs[].msgs[].body.parts[].headers[].name", Stripped),
    ("legs[].msgs[].body.parts[].headers[].value", Stripped),
    ("legs[].msgs[].headers[].name", Stripped),
    ("legs[].msgs[].headers[].value", Stripped),
    ("legs[].msgs[].headers[].wire", Stripped),
    ("legs[].msgs[].identities.from.digits", Stripped),
    ("legs[].msgs[].identities.from.uri", Stripped),
    ("legs[].msgs[].identities.from.user", Stripped),
    ("legs[].msgs[].identities.pai[].digits", Stripped),
    ("legs[].msgs[].identities.pai[].uri", Stripped),
    ("legs[].msgs[].identities.pai[].user", Stripped),
    ("legs[].msgs[].identities.ruri.digits", Stripped),
    ("legs[].msgs[].identities.ruri.uri", Stripped),
    ("legs[].msgs[].identities.ruri.user", Stripped),
    ("legs[].msgs[].identities.to.digits", Stripped),
    ("legs[].msgs[].identities.to.uri", Stripped),
    ("legs[].msgs[].identities.to.user", Stripped),
    ("legs[].msgs[].refer_to.replaces.call_id", Stripped),
    ("legs[].msgs[].refer_to.replaces.from_tag", Stripped),
    ("legs[].msgs[].refer_to.replaces.to_tag", Stripped),
    ("legs[].msgs[].refer_to.target.digits", Stripped),
    ("legs[].msgs[].refer_to.target.uri", Stripped),
    ("legs[].msgs[].refer_to.target.user", Stripped),
    ("legs[].msgs[].replaces.call_id", Stripped),
    ("legs[].msgs[].replaces.from_tag", Stripped),
    ("legs[].msgs[].replaces.to_tag", Stripped),
    ("legs[].msgs[].rseq", Stripped),
    ("legs[].msgs[].via[].branch", Stripped),
    ("legs[].msgs[].via[].received", Stripped),
    ("legs[].msgs[].via[].sent_by", Stripped),
    ("legs[].msgs[].via[].transport", Stripped),
];

/// The definition `$ref` names in `root`.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference.rsplit('/').next().unwrap_or_default();
            resolve(root, &root["$defs"][name])
        }
        None => schema,
    }
}

/// Every path under `schema` whose value is free text: a string that is no
/// `const` and no `enum`, and every map key.
fn text_paths(
    root: &Value,
    schema: &Value,
    path: &str,
    seen: &mut Vec<String>,
    out: &mut BTreeSet<String>,
) {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if seen.iter().filter(|r| *r == reference).count() > 1 {
            return;
        }
        seen.push(reference.to_string());
        text_paths(root, resolve(root, schema), path, seen, out);
        seen.pop();
        return;
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for branch in schema.get(key).and_then(Value::as_array).into_iter().flatten() {
            text_paths(root, branch, path, seen, out);
        }
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if types.contains(&"string") && schema.get("const").is_none() && schema.get("enum").is_none() {
        out.insert(path.to_string());
    }
    if let Some(items) = schema.get("items") {
        text_paths(root, items, &format!("{path}[]"), seen, out);
    }
    for (name, property) in
        schema.get("properties").and_then(Value::as_object).into_iter().flatten()
    {
        let at = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
        text_paths(root, property, &at, seen, out);
    }
    if let Some(values) = schema.get("additionalProperties").filter(|v| v.is_object()) {
        out.insert(format!("{path}{{key}}"));
        text_paths(root, values, &format!("{path}{{value}}"), seen, out);
    }
}

/// The schema's text fields, and those [`FIELDS`] classifies, are the same
/// set: a field upstream adds fails here until it is classified.
#[test]
fn every_text_field_of_the_document_is_classified() {
    let schema = schemars::schema_for!(Flows);
    let root = schema.as_value();
    let mut found = BTreeSet::new();
    text_paths(root, root, "", &mut Vec::new(), &mut found);
    let classified: BTreeSet<String> = FIELDS.iter().map(|(p, _)| p.to_string()).collect();
    assert_eq!(classified.len(), FIELDS.len(), "a field classified twice");
    let new: Vec<&String> = found.difference(&classified).collect();
    let gone: Vec<&String> = classified.difference(&found).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "unclassified: {new:#?}, no longer in the schema: {gone:#?}"
    );
}
