//! Per-call colours of the SIP plane: each Call-ID the SIP rows carry (in
//! [`SeqRow::conn`](crate::SeqRow::conn)) gets its own hue, in the order the
//! calls first appear, so a B2BUA's two legs, and every further call, read
//! apart at a glance. The hues are mid-tone, legible on a light and on a dark
//! background; past the last one the colours cycle.

use crate::{Item, RowKind};

/// The per-call hues, in the order calls take them.
pub(crate) const FLOW_PALETTE: &[&str] = &[
    "#2563eb", // blue
    "#ea580c", // orange
    "#16a34a", // green
    "#db2777", // pink
    "#0d9488", // teal
    "#a16207", // ochre
    "#7c3aed", // violet
    "#0891b2", // cyan
];

/// The Call-IDs of a doc's SIP rows, in first-seen order.
pub(crate) struct Flows<'a> {
    calls: Vec<&'a str>,
}

impl<'a> Flows<'a> {
    /// The calls `items` carry on their SIP rows, in render order.
    pub(crate) fn of(items: &[Item<'a>]) -> Self {
        let mut calls: Vec<&'a str> = Vec::new();
        for item in items {
            let Item::Row(row) = item else { continue };
            if let (RowKind::Sip { .. }, Some(call)) = (row.kind, row.conn.as_deref()) {
                if !calls.contains(&call) {
                    calls.push(call);
                }
            }
        }
        Self { calls }
    }

    /// The palette index of `call`, when a SIP row carries it.
    pub(crate) fn index(&self, call: &str) -> Option<usize> {
        self.calls.iter().position(|c| *c == call).map(|i| i % FLOW_PALETTE.len())
    }

    /// Every call with its hue, in first-seen order.
    pub(crate) fn legend(&self) -> impl Iterator<Item = (&'a str, &'static str)> + '_ {
        self.calls.iter().enumerate().map(|(i, c)| (*c, FLOW_PALETTE[i % FLOW_PALETTE.len()]))
    }
}

/// The arrowhead of each per-call hue, `ah-flow-{i}`.
pub(crate) fn marker_defs() -> String {
    FLOW_PALETTE
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "<marker id=\"ah-flow-{i}\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"3\" orient=\"auto\"><path d=\"M0,0 L7,3 L0,6 Z\" fill=\"{c}\"/></marker>"
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeqRow;

    fn sip(seq: u64, call: &str) -> SeqRow {
        SeqRow {
            at_ms: 0,
            seq,
            from: "a".into(),
            to: Some("b".into()),
            label: "INVITE".into(),
            detail: None,
            conn: Some(call.into()),
            kind: RowKind::Sip { delivered: true },
        }
    }

    #[test]
    fn each_call_takes_the_next_hue_in_first_seen_order_and_cycles() {
        let calls: Vec<String> = (0..10).map(|i| format!("call-{i}")).collect();
        let mut rows: Vec<SeqRow> =
            calls.iter().enumerate().map(|(i, c)| sip(i as u64, c)).collect();
        rows.push(sip(99, "call-1"));
        let items: Vec<Item<'_>> = rows.iter().map(Item::Row).collect();
        let flows = Flows::of(&items);
        assert_eq!(flows.index("call-0"), Some(0));
        assert_eq!(flows.index("call-1"), Some(1), "a call keeps its first hue");
        assert_eq!(flows.index("call-8"), Some(0), "past the palette the hues cycle");
        assert_eq!(flows.index("absent"), None);
        assert_eq!(flows.legend().count(), 10);
        let hues: std::collections::BTreeSet<_> = FLOW_PALETTE.iter().collect();
        assert!(hues.len() >= 6, "at least six distinct hues");
    }
}
