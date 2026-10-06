//! What a decision states for one header name on a message the stack sends:
//! the removal of every relayed line, the ordered lines that replace them, or
//! lines added only where the message carries none of the name.
//!
//! A header may legitimately occupy several lines (RFC 3261 §7.3.1), and for
//! some of them the line order is the meaning — each `Diversion` or
//! `History-Info` entry is one hop, most recent first. A decision therefore
//! states LINES, never a single value the generator would have to fold, and
//! the message carries one wire line per entry in the order given.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

/// Header name → what the decision states for it.
pub type SipHeaderUpdates = BTreeMap<String, HeaderUpdate>;

/// The decision's statement for one header name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderUpdate {
    /// Delete every relayed line of the name; the message carries none.
    Remove,
    /// Replace every relayed line with these, one wire line per entry, in
    /// this order. Empty states the name with no line: a removal.
    Set(Vec<String>),
    /// These lines, in this order, on a message that carries no line of the
    /// name; a message that carries one keeps its own and takes none of
    /// these. Empty adds nothing.
    Add(Vec<String>),
}

impl HeaderUpdate {
    /// A single-line statement.
    pub fn line(value: impl Into<String>) -> Self {
        HeaderUpdate::Set(vec![value.into()])
    }

    /// The lines this statement puts on the wire where it applies — none for
    /// a removal.
    pub fn lines(&self) -> &[String] {
        match self {
            HeaderUpdate::Remove => &[],
            HeaderUpdate::Set(lines) | HeaderUpdate::Add(lines) => lines,
        }
    }

    /// True iff the statement leaves the message with no line of the name.
    pub fn removes(&self) -> bool {
        match self {
            HeaderUpdate::Remove => true,
            HeaderUpdate::Set(lines) => lines.is_empty(),
            HeaderUpdate::Add(_) => false,
        }
    }

    /// True iff the statement yields to a line the message already carries.
    pub fn adds(&self) -> bool {
        matches!(self, HeaderUpdate::Add(_))
    }

    /// The single line of a one-line set or add; `None` for a removal or a
    /// multi-line statement.
    pub fn single(&self) -> Option<&str> {
        match self.lines() {
            [one] => Some(one),
            _ => None,
        }
    }
}

/// `Remove` is `null`; `Set` is the array of lines; `Add` is `{"add": [lines]}`.
impl Serialize for HeaderUpdate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            HeaderUpdate::Remove => s.serialize_none(),
            HeaderUpdate::Set(lines) => lines.serialize(s),
            HeaderUpdate::Add(lines) => {
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry("add", lines)?;
                map.end()
            }
        }
    }
}

/// `null` is a removal, an array is the lines, a bare string is the one-line
/// set — the form a decision adapter states a single-line header in — and
/// `{"add": lines}` (an array or a bare string) is an add.
impl<'de> Deserialize<'de> for HeaderUpdate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = HeaderUpdate;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("null, a string, an array of strings, or {\"add\": lines}")
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
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<HeaderUpdate, A::Error> {
                let mut lines = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key != "add" {
                        return Err(de::Error::unknown_field(&key, &["add"]));
                    }
                    lines = Some(match map.next_value::<HeaderUpdate>()? {
                        HeaderUpdate::Set(lines) => lines,
                        _ => return Err(de::Error::custom("`add` states lines")),
                    });
                }
                lines.map(HeaderUpdate::Add).ok_or_else(|| de::Error::missing_field("add"))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_set_is_a_removal_and_an_empty_add_is_not() {
        assert!(HeaderUpdate::Set(vec![]).removes());
        assert!(HeaderUpdate::Remove.removes());
        assert!(!HeaderUpdate::Add(vec![]).removes());
        assert!(HeaderUpdate::Add(vec!["a".into()]).adds());
        assert!(!HeaderUpdate::line("a").adds());
    }

    #[test]
    fn json_reads_null_string_array_and_add() {
        let m: SipHeaderUpdates = serde_json::from_str(
            r#"{"A": null, "B": "one", "C": ["x", "y"], "D": {"add": ["p", "q"]}, "E": {"add": "r"}}"#,
        )
        .unwrap();
        assert_eq!(m["A"], HeaderUpdate::Remove);
        assert_eq!(m["B"], HeaderUpdate::line("one"));
        assert_eq!(m["C"], HeaderUpdate::Set(vec!["x".into(), "y".into()]));
        assert_eq!(m["D"], HeaderUpdate::Add(vec!["p".into(), "q".into()]));
        assert_eq!(m["E"], HeaderUpdate::Add(vec!["r".into()]));
        assert!(serde_json::from_str::<HeaderUpdate>(r#"{"set": ["x"]}"#).is_err());
        assert!(serde_json::from_str::<HeaderUpdate>(r#"{"add": null}"#).is_err());
    }

    #[test]
    fn json_writes_null_arrays_and_add() {
        let mut m: SipHeaderUpdates = BTreeMap::new();
        m.insert("A".into(), HeaderUpdate::Remove);
        m.insert("B".into(), HeaderUpdate::line("one"));
        m.insert("C".into(), HeaderUpdate::Add(vec!["two".into()]));
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json, serde_json::json!({"A": null, "B": ["one"], "C": {"add": ["two"]}}));
        let back: SipHeaderUpdates = serde_json::from_value(json).unwrap();
        assert_eq!(back, m);
    }

    /// The call body's codec keeps every statement as stated.
    #[test]
    fn msgpack_round_trips_every_statement() {
        let mut m: SipHeaderUpdates = BTreeMap::new();
        m.insert("A".into(), HeaderUpdate::Remove);
        m.insert("B".into(), HeaderUpdate::Set(vec!["x".into(), "y".into()]));
        m.insert("C".into(), HeaderUpdate::Add(vec!["z".into()]));
        let bytes = rmp_serde::to_vec(&m).unwrap();
        let back: SipHeaderUpdates = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn single_reads_only_a_one_line_statement() {
        assert_eq!(HeaderUpdate::line("v").single(), Some("v"));
        assert_eq!(HeaderUpdate::Add(vec!["v".into()]).single(), Some("v"));
        assert_eq!(HeaderUpdate::Remove.single(), None);
        assert_eq!(HeaderUpdate::Set(vec!["a".into(), "b".into()]).single(), None);
    }
}
