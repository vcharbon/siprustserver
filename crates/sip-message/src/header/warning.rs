//! The `Warning` header (RFC 3261 §20.43): comma-separated
//! `warn-code SP warn-agent SP warn-text` values, the warn-agent naming the
//! element that added the warning (`hostport` or a pseudonym token).

/// The most bytes a warn-text built by [`warning_value`] holds, its quotes
/// excluded.
pub const WARN_TEXT_MAX: usize = 200;

/// One warning-value: `code SP agent SP warn-text`, the warn-text being the
/// first [`WARN_TEXT_MAX`] bytes of `text` in quotes, every byte outside
/// ASCII letters, digits, space and `.,:;-_()/=+'@[]<>` replaced by `?`. No
/// byte of `text` can close the quoted-string or the header line.
pub fn warning_value(code: u16, agent: &str, text: &str) -> String {
    let warn_text: String = text
        .bytes()
        .take(WARN_TEXT_MAX)
        .map(|byte| if is_plain(byte) { char::from(byte) } else { '?' })
        .collect();
    format!("{code} {agent} \"{warn_text}\"")
}

/// A byte a warn-text keeps as it is.
fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b" .,:;-_()/=+'@[]<>".contains(&byte)
}

/// The warn-agent of every warning-value of a `Warning` header line, in order.
/// A value that is not `code agent text` is skipped.
pub fn warn_agents(value: &str) -> Vec<&str> {
    crate::parser::custom::structured_headers::split_top_level_commas(value)
        .into_iter()
        .filter_map(|item| {
            let mut words = item.split_ascii_whitespace();
            let code = words.next()?;
            let agent = words.next()?;
            (code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit())).then_some(agent)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::warn_agents;

    #[test]
    fn every_warning_names_its_agent() {
        assert_eq!(
            warn_agents("399 proxy.example.com \"busy\", 307 isi.example.org \"a, b\""),
            ["proxy.example.com", "isi.example.org"]
        );
        assert!(warn_agents("not a warning").is_empty());
    }
}
