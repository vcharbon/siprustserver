//! A refusal reason writes every value it read from the message as a debug
//! string (`{:?}`), the form its key leaves out (`sip_message::error::reason_key`).
//! This guard reads every `SipParseError::new(format!(…))` of the crate's
//! sources and refuses a placeholder written any other way, unless its argument
//! is listed in [`ALLOWED`]: a number, a fixed name the crate chose, a token of
//! a closed grammar that holds no `"` or space, or a reason the crate built.

use std::path::{Path, PathBuf};

/// `(argument as written, why it may be written plainly)`.
const ALLOWED: &[(&str, &str)] = &[
    // Reasons the crate built, themselves held to this guard.
    ("reason", "a reason built by the crate"),
    ("e.reason", "a reason built by the crate"),
    ("what", "fixed words the crate chose"),
    ("why", "fixed words the crate chose"),
    // Names the crate states.
    ("header", "a header name the crate states"),
    ("K::name()", "a header kind's canonical name"),
    ("K::name().as_wire_str()", "a header kind's canonical name"),
    ("name", "a header name the crate states or a fixed CR/LF label"),
    ("shown", "a header as `reason_text::header` writes it, or a debug string"),
    (
        "crate::error::reason_text::header(&HeaderName::of(&long.name))",
        "a header as `reason_text::header` writes it",
    ),
    ("field", "a field name the crate states"),
    ("N", "a field count the crate states"),
    ("BRANCH_MAGIC_COOKIE", "a constant"),
    ("crate::char_class::utf8_note(&call_id)", "fixed words the crate chose"),
    ("crate::char_class::utf8_note(rest)", "fixed words the crate chose"),
    ("line_holding(header_block, at)", "a header name written as a debug string"),
    // Tokens of a closed grammar: no `"`, no space.
    ("method", "a method, a token (RFC 3261 §25.1)"),
    ("cseq_method", "a method, a token (RFC 3261 §25.1)"),
    // Numbers.
    ("status", "a status code"),
    ("p", "a port number"),
    ("value", "a number read as u32"),
    ("min", "a bound"),
    ("lines", "a count"),
    ("uri_length", "a length"),
    ("header_len", "a length"),
    ("declared", "a length"),
    ("available", "a length"),
    ("end", "an offset"),
    ("at", "an offset"),
    ("s.pos()", "an offset"),
    ("self.pos", "an offset"),
    ("self.at", "an offset"),
    ("raw_name.start", "an offset"),
    ("long.name.len() + 2 + long.value.len()", "a length"),
    ("limits.max_header_length", "a bound"),
    ("max_length", "a bound"),
    ("limits.max_header_lines", "a bound"),
    ("self.header_lines", "a bound"),
    ("self.uri_length", "a bound"),
    ("self.header_length", "a bound"),
    ("self.max_datagram", "a bound"),
    ("K::MIN", "a bound"),
    ("K::MAX", "a bound"),
    ("cs.len()", "a count"),
    ("block.lines", "a count"),
    ("uri.len()", "a length"),
    ("payload.len()", "a length"),
    ("limits.max_uri_length", "a bound"),
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the source tree reads") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The string literal opening `text` (after `"`), its escapes and line
/// continuations kept as written, and the rest after its closing quote.
fn literal(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\\' => i += 2,
            b'"' => return Some((text.get(..i)?, text.get(i + 1..)?)),
            _ => i += 1,
        }
    }
    None
}

/// The arguments after a format literal, up to the call's closing paren, split
/// on top-level commas and trimmed.
fn arguments(rest: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                current.push(c);
                while let Some(c) = chars.next() {
                    current.push(c);
                    if c == '\\' {
                        current.extend(chars.next());
                    } else if c == '"' {
                        break;
                    }
                }
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(c);
            }
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            c => current.push(c),
        }
    }
    args.push(current.trim().to_string());
    args.into_iter().filter(|a| !a.is_empty()).collect()
}

/// Each placeholder of `literal` as `(argument, spec)`: a named one by its
/// name or the expression a `name = expr` argument gives it, a positional one
/// by the next positional argument.
fn placeholders(literal: &str, args: &[String]) -> Vec<(String, String)> {
    let positional: Vec<&String> = args.iter().filter(|a| !is_named(a)).collect();
    let mut next = positional.iter();
    let mut found = Vec::new();
    let text = literal.replace("{{", "").replace("}}", "");
    let mut rest = text.as_str();
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else { break };
        let inner = &rest[open + 1..open + close];
        let (name, spec) = inner.split_once(':').unwrap_or((inner, ""));
        let argument = if name.is_empty() {
            next.next().map_or_else(|| "?".to_string(), |a| a.to_string())
        } else {
            args.iter()
                .find_map(|a| {
                    a.split_once('=')
                        .filter(|(n, _)| n.trim() == name && is_named(a))
                        .map(|(_, e)| e.trim().to_string())
                })
                .unwrap_or_else(|| name.to_string())
        };
        found.push((argument, spec.to_string()));
        rest = &rest[open + close + 1..];
    }
    found
}

fn is_named(argument: &str) -> bool {
    argument.split_once('=').is_some_and(|(n, e)| {
        !e.starts_with('=') && n.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether `spec` writes the value as a debug string or as a number.
fn safe_spec(spec: &str) -> bool {
    spec.starts_with('?') || spec.starts_with("#?") || spec.ends_with('x') || spec.ends_with('X')
}

/// Every placeholder of the `SipParseError::new(format!(…))` calls of `source`
/// outside a test module, as `(argument, spec, literal)`.
fn reason_placeholders(source: &str) -> Vec<(String, String, String)> {
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    let mut out = Vec::new();
    let mut rest = code;
    while let Some(at) = rest.find("SipParseError::new(") {
        rest = &rest[at + "SipParseError::new(".len()..];
        let trimmed = rest.trim_start();
        let Some(call) = trimmed.strip_prefix("format!(") else { continue };
        let Some((lit, after)) = call.trim_start().strip_prefix('"').and_then(literal) else {
            continue;
        };
        let args = arguments(after.trim_start().strip_prefix(',').unwrap_or(""));
        for (argument, spec) in placeholders(lit, &args) {
            out.push((argument, spec, lit.chars().take(80).collect()));
        }
    }
    out
}

/// The placeholders of `source` that are neither debug, hex nor listed.
fn unlisted(source: &str) -> Vec<String> {
    reason_placeholders(source)
        .into_iter()
        .filter(|(argument, spec, _)| {
            !safe_spec(spec) && !ALLOWED.iter().any(|(a, _)| a == argument)
        })
        .map(|(argument, _, lit)| format!("{{{argument}}} in {lit:?}"))
        .collect()
}

/// Every source of the crate, by its path under `src/`.
fn crate_sources() -> Vec<(String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    files
        .into_iter()
        .map(|file| {
            let relative = file.strip_prefix(&src).expect("under src").display().to_string();
            (relative, std::fs::read_to_string(&file).expect("a source reads"))
        })
        .collect()
}

#[test]
fn the_reader_names_each_placeholder_by_its_argument() {
    let source = r#"x(SipParseError::new(format!("a {} b {n:?} c {named} d {:02x}", one.len(), two, named = three(4))))"#;
    let lit = "a {} b {n:?} c {named} d {:02x}";
    let args = arguments(source.split_once(lit).expect("lit").1.trim_start_matches(['"', ',']));
    assert_eq!(args, ["one.len()", "two", "named = three(4)"]);
    let found = placeholders(lit, &args);
    let names: Vec<(&str, &str)> = found.iter().map(|(a, s)| (a.as_str(), s.as_str())).collect();
    assert_eq!(names, [("one.len()", ""), ("n", "?"), ("three(4)", ""), ("two", "02x")]);
    assert_eq!(
        unlisted(source),
        [
            "{one.len()} in \"a {} b {n:?} c {named} d {:02x}\"",
            "{three(4)} in \"a {} b {n:?} c {named} d {:02x}\""
        ]
    );
}

#[test]
fn every_refusal_reason_writes_what_it_read_as_a_debug_string() {
    let found: Vec<String> = crate_sources()
        .iter()
        .flat_map(|(path, source)| {
            unlisted(source).into_iter().map(move |u| format!("{path}: {u}"))
        })
        .collect();
    assert!(found.is_empty(), "write these with {{:?}} or list them:\n{}", found.join("\n"));
}

#[test]
fn every_listed_argument_is_written_somewhere() {
    let written: Vec<String> = crate_sources()
        .iter()
        .flat_map(|(_, source)| reason_placeholders(source).into_iter().map(|(a, _, _)| a))
        .collect();
    let stale: Vec<&str> =
        ALLOWED.iter().map(|(a, _)| *a).filter(|a| !written.iter().any(|w| w == a)).collect();
    assert!(stale.is_empty(), "listed and written nowhere: {stale:?}");
}
