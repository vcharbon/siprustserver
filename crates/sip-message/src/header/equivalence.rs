//! Whether two wire FORMS of one header say the same thing.
//!
//! RFC 3261 §7.3.1 gives a header two freedoms a byte comparison denies it: the
//! separators of a list carry optional linear whitespace (`COMMA = SWS "," SWS`,
//! `SEMI`, `EQUAL`, `LAQUOT` — §25.1), and several rows of one comma-list header
//! combine into one row "without changing the semantics". So `Q.850; cause=16`
//! and `Q.850;cause=16` are one header, and two `Supported` rows are one header
//! with the folded row that carries their items in order.
//!
//! The comparison stays a SEQUENCE comparison: `Via`, `Route` and `Record-Route`
//! are comma lists whose order is the routing itself, and `History-Info` entries
//! are ordered by their index, so an order-insensitive equivalence would accept a
//! reversed chain. Whitespace and row layout are the only freedoms taken.

use super::name::HeaderName;

/// The items `rows` state, in wire order, each with its insignificant
/// whitespace removed — the form two wire spellings of one header share.
///
/// Rows fold: the items of every row join into one sequence, over the separator
/// this header's own grammar declares ([`HeaderName::item_separator_of`]).
pub fn canonical_header_items(name: &str, rows: &[&str]) -> Vec<String> {
    let separator = HeaderName::item_separator_of(name);
    rows.iter().flat_map(|row| separator.split(row)).map(canonical_item).collect()
}

/// Whether the two sets of rows are the same header value.
pub fn header_forms_equivalent(name: &str, left: &[&str], right: &[&str]) -> bool {
    canonical_header_items(name, left) == canonical_header_items(name, right)
}

/// One list item with its insignificant whitespace gone: runs of LWS collapse to
/// one space, and the space either side of a structural separator (`; = < >`)
/// disappears. A quoted-string is data and survives byte for byte.
fn canonical_item(item: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(item.len());
    let mut in_quote = false;
    let mut escaped = false;
    let mut pending_space = false;
    let mut after_separator = true;
    // Byte scan: every character this reads for structure is ASCII, so a
    // multi-byte sequence's bytes all fall to the default arm and are copied
    // through intact.
    for &c in item.as_bytes() {
        if in_quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_quote = false;
                after_separator = false;
            }
            continue;
        }
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => pending_space = true,
            b';' | b'=' | b'<' | b'>' => {
                out.push(c);
                pending_space = false;
                after_separator = true;
            }
            _ => {
                if pending_space && !after_separator && !out.is_empty() {
                    out.push(b' ');
                }
                pending_space = false;
                after_separator = false;
                in_quote = c == b'"';
                out.push(c);
            }
        }
    }
    String::from_utf8(out).expect("only ASCII layout bytes are dropped, so the sequences survive")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_around_a_separator_is_not_a_difference() {
        for (a, b) in [
            ("Q.850; cause=16", "Q.850;cause=16"),
            ("Q.850 ;cause=16", "Q.850;cause=16"),
            ("Q.850 ;cause=16 ;text=\"Terminated\"", "Q.850;cause=16;text=\"Terminated\""),
            ("INVITE, ACK, BYE", "INVITE,ACK,BYE"),
            ("\"Xxxxx\" <sip:a@h>;index=1", "\"Xxxxx\"<sip:a@h>;index=1"),
            ("text = \"a\"", "text=\"a\""),
        ] {
            assert!(header_forms_equivalent("Reason", &[a], &[b]), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn several_rows_of_one_header_are_the_folded_row() {
        assert!(header_forms_equivalent("Supported", &["100rel", "timer"], &["100rel, timer"]));
        assert!(header_forms_equivalent("Supported", &["100rel, timer"], &["100rel", "timer"]));
        assert!(header_forms_equivalent(
            "History-Info",
            &["<sip:a@h>;index=1", "<sip:b@h>;index=1.1"],
            &["<sip:a@h>;index=1, <sip:b@h>;index=1.1"],
        ));
    }

    #[test]
    fn a_real_value_difference_still_differs() {
        // The residue §6.4 exists to catch: our stack advertising MORE.
        assert!(!header_forms_equivalent("Allow", &["INVITE, ACK, OPTIONS"], &["INVITE,ACK"]));
        assert!(!header_forms_equivalent("Reason", &["Q.850; cause=127"], &["Q.850;cause=16"]));
        assert!(!header_forms_equivalent(
            "Reason",
            &["Q.850;cause=16"],
            &["Q.850;cause=16;text=\"x\""]
        ));
        // The national prefix hiding inside an entry the whitespace makes equal.
        assert!(!header_forms_equivalent(
            "History-Info",
            &["\"N\" <sip:0009001@h>;index=1", "<sip:0033000900004@h>;index=1.1"],
            &["\"N\"<sip:0009001@h>;index=1, <sip:+33000900004@h>;index=1.1"],
        ));
    }

    #[test]
    fn order_is_part_of_the_value() {
        assert!(!header_forms_equivalent("Allow", &["INVITE, ACK"], &["ACK, INVITE"]));
        assert!(!header_forms_equivalent(
            "Via",
            &["SIP/2.0/UDP a:5060, SIP/2.0/UDP b:5060"],
            &["SIP/2.0/UDP b:5060, SIP/2.0/UDP a:5060"],
        ));
    }

    #[test]
    fn a_separator_inside_quotes_or_angles_is_data() {
        assert!(!header_forms_equivalent("Reason", &["Q.850;text=\"a,b\""], &["Q.850;text=\"a\""]));
        assert_eq!(canonical_header_items("Reason", &["Q.850;text=\"a, b\""]).len(), 1);
        assert_eq!(canonical_header_items("Contact", &["<sip:a@h;p=1,2>"]).len(), 1);
        // Whitespace inside a quoted string is the caller's data, not layout.
        assert!(!header_forms_equivalent(
            "Reason",
            &["Q.850;text=\"a  b\""],
            &["Q.850;text=\"a b\""]
        ));
    }

    #[test]
    fn a_header_with_no_structural_separator_keeps_its_spacing_as_one_space() {
        assert!(header_forms_equivalent("User-Agent", &["Foo/1.0  Bar/2.0"], &["Foo/1.0 Bar/2.0"]));
        assert!(!header_forms_equivalent("User-Agent", &["Foo/1.0"], &["Foo/2.0"]));
    }

    #[test]
    fn the_privacy_family_splits_on_its_own_separator() {
        assert!(header_forms_equivalent("Privacy", &["id; user"], &["id;user"]));
        assert!(!header_forms_equivalent("Privacy", &["id;user"], &["user;id"]));
    }
}
