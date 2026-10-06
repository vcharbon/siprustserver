//! The decision's header statements ([`call::header_update`]) as the
//! `(name, line-or-removal)` pairs a message generator consumes.

pub use call::header_update::{HeaderUpdate, SipHeaderUpdates};

/// The sets and removals of the map as the `(name, line-or-removal)` pairs a
/// message generator consumes: one pair per line of a set, in the set's order,
/// and one `(name, None)` per removal. Names keep the map's order. An add is
/// not among them: it yields to the message as built ([`header_adds`]).
pub fn header_lines(updates: &SipHeaderUpdates) -> Vec<(String, Option<String>)> {
    updates
        .iter()
        .filter(|(_, update)| !update.adds())
        .flat_map(|(name, update)| -> Vec<(String, Option<String>)> {
            if update.removes() {
                vec![(name.clone(), None)]
            } else {
                update.lines().iter().map(|l| (name.clone(), Some(l.clone()))).collect()
            }
        })
        .collect()
}

/// The adds of the map, `(name, lines)` in the map's order: what a builder
/// states on the message it built only where that message carries none of
/// the name. An add with no line is none.
pub fn header_adds(updates: &SipHeaderUpdates) -> Vec<(String, Vec<String>)> {
    updates
        .iter()
        .filter_map(|(name, update)| match update {
            HeaderUpdate::Add(lines) if !lines.is_empty() => Some((name.clone(), lines.clone())),
            _ => None,
        })
        .collect()
}

/// The adds of a payload's `update_headers` object ([`header_adds`] of it).
pub fn payload_adds(update_headers: Option<&serde_json::Value>) -> Vec<(String, Vec<String>)> {
    update_headers
        .filter(|v| v.is_object())
        .and_then(|v| serde_json::from_value::<SipHeaderUpdates>(v.clone()).ok())
        .map(|m| header_adds(&m))
        .unwrap_or_default()
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

/// The stated headers of a final a decision authors (a reject, a redirect)
/// from its payload's `update_headers`: the adds, on that final, yielding to
/// the lines it carries as built; `None` where it adds nothing.
pub fn final_adds(
    update_headers: Option<&serde_json::Value>,
) -> Option<call::features::StatedHeaders> {
    let adds = payload_adds(update_headers);
    (!adds.is_empty()).then(|| call::features::StatedHeaders {
        originator_finals: adds
            .into_iter()
            .map(|(name, lines)| (name, HeaderUpdate::Add(lines)))
            .collect(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
    fn an_add_is_no_generator_pair_and_an_empty_add_is_no_add() {
        let mut m: SipHeaderUpdates = BTreeMap::new();
        m.insert("A".into(), HeaderUpdate::Add(vec!["x".into()]));
        m.insert("B".into(), HeaderUpdate::Add(vec![]));
        m.insert("C".into(), HeaderUpdate::line("c"));
        assert_eq!(header_lines(&m), vec![("C".to_string(), Some("c".to_string()))]);
        assert_eq!(header_adds(&m), vec![("A".to_string(), vec!["x".to_string()])]);
        let payload = serde_json::json!({"A": {"add": ["x"]}, "C": "c"});
        assert_eq!(payload_adds(Some(&payload)), vec![("A".to_string(), vec!["x".to_string()])]);
    }
}
