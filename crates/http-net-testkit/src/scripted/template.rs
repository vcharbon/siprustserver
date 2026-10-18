//! The closed template grammar: literal text, `${bind:NAME}`,
//! `${capture:NAME}` and `${continuation}`.

use std::collections::BTreeMap;

use super::program::HttpBindings;

/// Captured values by name, each the raw text of one JSON scalar.
pub(super) type Captures = BTreeMap<String, String>;

/// One parsed piece of a template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Piece {
    Lit(String),
    Bind(String),
    Capture(String),
    Continuation,
}

/// Parse `text`; the error is the offending `${…}` text.
pub(super) fn parse(text: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            pieces.push(Piece::Lit(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(rest[start..].to_string());
        };
        let inner = &after[..end];
        let piece = if inner == "continuation" {
            Piece::Continuation
        } else if let Some(name) = inner.strip_prefix("bind:").filter(|n| is_name(n)) {
            Piece::Bind(name.to_string())
        } else if let Some(name) = inner.strip_prefix("capture:").filter(|n| is_name(n)) {
            Piece::Capture(name.to_string())
        } else {
            return Err(format!("${{{inner}}}"));
        };
        pieces.push(piece);
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Lit(rest.to_string()));
    }
    Ok(pieces)
}

fn is_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
}

/// Render `pieces`; the error names what could not be resolved.
pub(super) fn render(
    pieces: &[Piece],
    bindings: &HttpBindings,
    captures: &Captures,
    continuation: &str,
) -> Result<String, String> {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Lit(text) => out.push_str(text),
            Piece::Bind(name) => out.push_str(
                bindings.get(name).ok_or_else(|| format!("${{bind:{name}}} has no binding"))?,
            ),
            Piece::Capture(name) => out.push_str(
                captures
                    .get(name)
                    .ok_or_else(|| format!("${{capture:{name}}} was not captured"))?,
            ),
            Piece::Continuation => out.push_str(continuation),
        }
    }
    Ok(out)
}

/// `text` rendered for display in a diagnostic: bindings resolved, the other
/// placeholders kept as written.
pub(super) fn show(text: &str, bindings: &HttpBindings) -> String {
    match parse(text) {
        Ok(pieces) => pieces
            .iter()
            .map(|p| match p {
                Piece::Lit(t) => t.clone(),
                Piece::Bind(n) => {
                    bindings.get(n).map_or_else(|| format!("${{bind:{n}}}"), str::to_string)
                }
                Piece::Capture(n) => format!("${{capture:{n}}}"),
                Piece::Continuation => "${continuation}".to_string(),
            })
            .collect(),
        Err(_) => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_placeholders_and_refuses_the_rest() {
        assert_eq!(
            parse(r#"{"a":"${bind:x}","b":${capture:n},"c":"${continuation}"}"#).unwrap(),
            vec![
                Piece::Lit(r#"{"a":""#.into()),
                Piece::Bind("x".into()),
                Piece::Lit(r#"","b":"#.into()),
                Piece::Capture("n".into()),
                Piece::Lit(r#","c":""#.into()),
                Piece::Continuation,
                Piece::Lit(r#""}"#.into()),
            ]
        );
        assert_eq!(parse("${num:1}").unwrap_err(), "${num:1}");
        assert_eq!(parse("a ${bind:x").unwrap_err(), "${bind:x");
        assert_eq!(parse("${bind:}").unwrap_err(), "${bind:}");
        assert_eq!(parse("plain $ {x}").unwrap(), vec![Piece::Lit("plain $ {x}".into())]);
    }
}
