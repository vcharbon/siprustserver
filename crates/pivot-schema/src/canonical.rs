//! Canonical pivot serialization (`PCAP2TEST_PIVOT_V3.md` §2.1): keys sorted
//! lexically at every level, two-space indent, one trailing newline.
//!
//! Serialization is FORMATTER-owned. Generator output and hand edits are
//! normalised through here before any diff, so no emitter reproduces a struct's
//! declaration order by hand and no consumer depends on one surviving a
//! round-trip. Sorting is done here rather than left to the `serde_json` map
//! implementation, so the output is identical whether or not a build enables
//! that crate's insertion-order feature.

use serde::Serialize;
use serde_json::Value;

/// Serialize `value` canonically. Infallible for any type whose `Serialize` is
/// infallible, which every model in this crate is.
pub fn format<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    Ok(format_json(&serde_json::to_value(value)?))
}

/// Canonicalize an already-parsed JSON document.
pub fn format_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out.push('\n');
    out
}

/// Canonicalize a JSON document from its text. Parse errors surface: a
/// formatter that silently passes through malformed input is not a contract.
pub fn format_str(text: &str) -> Result<String, serde_json::Error> {
    Ok(format_json(&serde_json::from_str::<Value>(text)?))
}

/// Serialize `value` as ONE JSON Lines record: the same key order, on one line,
/// with no newline of its own — the stream owns the line breaks.
///
/// A record kind written as a stream (`recording/<leg>.jsonl`) still states one
/// byte form, so a mirror in another language emits the same bytes.
pub fn format_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    write_compact(&mut out, &serde_json::to_value(value)?);
    Ok(out)
}

fn write_compact(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (n, key) in keys.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                write_scalar(out, &Value::String((*key).clone()));
                out.push(':');
                write_compact(out, &map[*key]);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                write_compact(out, item);
            }
            out.push(']');
        }
        scalar => write_scalar(out, scalar),
    }
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push_str("{\n");
            for (n, key) in keys.iter().enumerate() {
                indent(out, depth + 1);
                write_scalar(out, &Value::String((*key).clone()));
                out.push_str(": ");
                write_value(out, &map[*key], depth + 1);
                out.push_str(if n + 1 == keys.len() { "\n" } else { ",\n" });
            }
            indent(out, depth);
            out.push('}');
        }
        Value::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (n, item) in items.iter().enumerate() {
                indent(out, depth + 1);
                write_value(out, item, depth + 1);
                out.push_str(if n + 1 == items.len() { "\n" } else { ",\n" });
            }
            indent(out, depth);
            out.push(']');
        }
        Value::Object(_) => out.push_str("{}"),
        Value::Array(_) => out.push_str("[]"),
        scalar => write_scalar(out, scalar),
    }
}

/// Scalars go through `serde_json` itself, so string escaping is the one
/// implementation the rest of the tree already trusts.
fn write_scalar(out: &mut String, value: &Value) {
    out.push_str(&serde_json::to_string(value).expect("a JSON scalar serializes"));
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_sort_at_every_level_and_the_document_ends_in_one_newline() {
        let text = format_json(&json!({ "b": 1, "a": { "z": [1, 2], "y": true } }));
        assert_eq!(
            text,
            "{\n  \"a\": {\n    \"y\": true,\n    \"z\": [\n      1,\n      2\n    ]\n  },\n  \"b\": 1\n}\n"
        );
    }

    #[test]
    fn formatting_is_idempotent() {
        let once = format_json(&json!({ "b": [{ "d": 4, "c": 3 }], "a": "x" }));
        assert_eq!(format_str(&once).unwrap(), once);
    }

    #[test]
    fn empty_collections_stay_compact_and_strings_keep_their_escapes() {
        assert_eq!(format_json(&json!({ "a": [], "b": {} })), "{\n  \"a\": [],\n  \"b\": {}\n}\n");
        assert_eq!(format_json(&json!("a\"b\\c\nd")), "\"a\\\"b\\\\c\\nd\"\n");
    }

    #[test]
    fn non_ascii_stays_literal_so_the_typescript_emitter_agrees() {
        assert_eq!(format_json(&json!("café — ok")), "\"café — ok\"\n");
    }

    #[test]
    fn a_line_holds_the_document_key_order_on_one_line_and_ends_without_a_newline() {
        let line = format_line(&json!({ "b": 1, "a": { "z": [1, 2], "y": true } })).unwrap();
        assert_eq!(line, r#"{"a":{"y":true,"z":[1,2]},"b":1}"#);
        // Same keys, same order, same escaping as the document form.
        assert_eq!(
            format_str(&line).unwrap(),
            format_json(&json!({ "b": 1, "a": { "z": [1, 2], "y": true } }))
        );
        assert_eq!(format_line(&json!({ "a": [], "b": {} })).unwrap(), r#"{"a":[],"b":{}}"#);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_passthrough() {
        assert!(format_str("{\"a\": }").is_err());
    }
}
