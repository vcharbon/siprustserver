//! Whitespace in SIP text: SP and HTAB alone (RFC 3261 §25.1 `WSP`), what LWS
//! is once a fold is unfolded. A value's edges are trimmed of these two bytes
//! and of nothing else, so VT, FF, CR, LF and NBSP stay in the value for the
//! gates and readers past the trim. SDP fields split on the same two bytes
//! (RFC 4566 §5 separates them with SP), and a part head of a multipart body
//! reads the same whitespace as a message head. XML text (MSCML) follows XML's
//! own whitespace rule, not this one.

/// Whether `b` is SP or HTAB.
pub(crate) fn is_wsp(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

fn is_wsp_char(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// `text` without the SP and HTAB at its edges.
pub(crate) fn trim(text: &str) -> &str {
    text.trim_matches(is_wsp_char)
}

/// `text` without the SP and HTAB it starts with.
pub(crate) fn trim_start(text: &str) -> &str {
    text.trim_start_matches(is_wsp_char)
}

/// `text` without the SP and HTAB it ends with.
pub(crate) fn trim_end(text: &str) -> &str {
    text.trim_end_matches(is_wsp_char)
}

/// The runs of `text` that SP and HTAB separate, empty runs left out.
pub(crate) fn split(text: &str) -> impl DoubleEndedIterator<Item = &str> {
    text.split(is_wsp_char).filter(|run| !run.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sp_and_htab_are_whitespace() {
        assert_eq!(trim(" \tabc\t "), "abc");
        for edge in ["\u{b}", "\u{c}", "\r", "\n", "\u{a0}"] {
            let text = format!("{edge}abc{edge}");
            assert_eq!(trim(&text), text, "{edge:?} stays");
        }
        assert_eq!(trim_start(" a "), "a ");
        assert_eq!(trim_end(" a "), " a");
        assert_eq!(split("a  b\tc\u{a0}d").collect::<Vec<_>>(), ["a", "b", "c\u{a0}d"]);
    }
}
