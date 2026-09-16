//! What a decision states for one header name on a message the stack
//! authors: the removal of every relayed line, or the ordered lines that
//! replace them.
//!
//! A header may legitimately occupy several lines (RFC 3261 §7.3.1), and for
//! some of them the line order is the meaning — each `Diversion` or
//! `History-Info` entry is one hop, most recent first. A decision therefore
//! states LINES, never a single value the generator would have to fold, and
//! the mint writes one wire line per entry in the order given.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Header name → what the decision states for it. The name is the decision's
/// whatever the variant: no relayed or configured copy of it rides beside.
pub type SipHeaderUpdates = BTreeMap<String, HeaderUpdate>;

/// The decision's statement for one header name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderUpdate {
    /// Delete every relayed line of the name; the message carries none.
    Remove,
    /// Replace every relayed line with these, one wire line per entry, in
    /// this order. Empty states the name with no line, which the mint treats
    /// as [`HeaderUpdate::Remove`].
    Set(Vec<String>),
}

impl HeaderUpdate {
    /// A single-line statement.
    pub fn line(value: impl Into<String>) -> Self {
        HeaderUpdate::Set(vec![value.into()])
    }

    /// The lines this statement puts on the wire — none for a removal.
    pub fn lines(&self) -> &[String] {
        match self {
            HeaderUpdate::Remove => &[],
            HeaderUpdate::Set(lines) => lines,
        }
    }

    /// True iff the statement leaves the message with no line of the name.
    pub fn removes(&self) -> bool {
        self.lines().is_empty()
    }

    /// The single line of a one-line statement; `None` for a removal or a
    /// multi-line set.
    pub fn single(&self) -> Option<&str> {
        match self.lines() {
            [one] => Some(one),
            _ => None,
        }
    }
}

/// The map as the `(name, line-or-removal)` pairs a message generator
/// consumes: one pair per line of a set, in the set's order, and one
/// `(name, None)` per removal. Names keep the map's order.
pub fn header_lines(updates: &SipHeaderUpdates) -> Vec<(String, Option<String>)> {
    updates
        .iter()
        .flat_map(|(name, update)| -> Vec<(String, Option<String>)> {
            if update.removes() {
                vec![(name.clone(), None)]
            } else {
                update.lines().iter().map(|l| (name.clone(), Some(l.clone()))).collect()
            }
        })
        .collect()
}

/// The `(name, line-or-removal)` pairs of a payload's `update_headers` object
/// ([`header_lines`] of it), or none when the payload carries no readable
/// object — the form a rule reads a fold's decision back in.
pub fn payload_lines(update_headers: Option<&serde_json::Value>) -> Vec<(String, Option<String>)> {
    update_headers
        .filter(|v| v.is_object())
        .and_then(|v| serde_json::from_value::<SipHeaderUpdates>(v.clone()).ok())
        .map(|m| header_lines(&m))
        .unwrap_or_default()
}

/// `Remove` is `null`; `Set` is the array of lines.
impl Serialize for HeaderUpdate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            HeaderUpdate::Remove => s.serialize_none(),
            HeaderUpdate::Set(lines) => lines.serialize(s),
        }
    }
}

/// `null` is a removal, an array is the lines, and a bare string is the
/// one-line set — the form a decision adapter states a single-line header in.
impl<'de> Deserialize<'de> for HeaderUpdate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = HeaderUpdate;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("null, a string, or an array of strings")
            }
            fn visit_none<E: de::Error>(self) -> Result<HeaderUpdate, E> {
                Ok(HeaderUpdate::Remove)
            }
            fn visit_unit<E: de::Error>(self) -> Result<HeaderUpdate, E> {
                Ok(HeaderUpdate::Remove)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<HeaderUpdate, D2::Error> {
                d.deserialize_any(V)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<HeaderUpdate, E> {
                Ok(HeaderUpdate::line(v))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<HeaderUpdate, E> {
                Ok(HeaderUpdate::Set(vec![v]))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<HeaderUpdate, A::Error> {
                let mut lines = Vec::new();
                while let Some(l) = seq.next_element::<String>()? {
                    lines.push(l);
                }
                Ok(HeaderUpdate::Set(lines))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_keeps_its_lines_in_order_and_a_removal_has_none() {
        let mut m: SipHeaderUpdates = BTreeMap::new();
        m.insert("Diversion".into(), HeaderUpdate::Set(vec!["<sip:b>".into(), "<sip:a>".into()]));
        m.insert("Privacy".into(), HeaderUpdate::Remove);
        m.insert("Reason".into(), HeaderUpdate::line("Q.850;cause=16"));
        assert_eq!(
            header_lines(&m),
            vec![
                ("Diversion".to_string(), Some("<sip:b>".to_string())),
                ("Diversion".to_string(), Some("<sip:a>".to_string())),
                ("Privacy".to_string(), None),
                ("Reason".to_string(), Some("Q.850;cause=16".to_string())),
            ]
        );
    }

    #[test]
    fn an_empty_set_is_a_removal() {
        let mut m: SipHeaderUpdates = BTreeMap::new();
        m.insert("Privacy".into(), HeaderUpdate::Set(vec![]));
        assert!(m["Privacy"].removes());
        assert_eq!(header_lines(&m), vec![("Privacy".to_string(), None)]);
    }

    #[test]
    fn json_reads_null_string_and_array() {
        let m: SipHeaderUpdates =
            serde_json::from_str(r#"{"A": null, "B": "one", "C": ["x", "y"]}"#).unwrap();
        assert_eq!(m["A"], HeaderUpdate::Remove);
        assert_eq!(m["B"], HeaderUpdate::line("one"));
        assert_eq!(m["C"], HeaderUpdate::Set(vec!["x".into(), "y".into()]));
    }

    #[test]
    fn json_writes_null_and_arrays() {
        let mut m: SipHeaderUpdates = BTreeMap::new();
        m.insert("A".into(), HeaderUpdate::Remove);
        m.insert("B".into(), HeaderUpdate::line("one"));
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json, serde_json::json!({"A": null, "B": ["one"]}));
        let back: SipHeaderUpdates = serde_json::from_value(json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn single_reads_only_a_one_line_set() {
        assert_eq!(HeaderUpdate::line("v").single(), Some("v"));
        assert_eq!(HeaderUpdate::Remove.single(), None);
        assert_eq!(HeaderUpdate::Set(vec!["a".into(), "b".into()]).single(), None);
    }
}
