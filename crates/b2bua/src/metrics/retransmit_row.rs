//! A `b2bua_retransmits_total` row: the label values one retransmission is
//! counted under.

/// A `b2bua_retransmits_total` row: method uppercased, the code's digits for
/// a response.
pub(super) struct RetransmitRow {
    ladder: String,
    method: String,
    code: Option<String>,
}

impl RetransmitRow {
    pub(super) fn as_refs(&self) -> Vec<&str> {
        let mut row = vec![self.ladder.as_str(), self.method.as_str()];
        row.extend(self.code.as_deref());
        row
    }
}

pub(super) fn retransmit_row(ladder: &str, method: &str, code: Option<u16>) -> RetransmitRow {
    RetransmitRow {
        ladder: ladder.to_owned(),
        method: method.to_ascii_uppercase(),
        code: code.map(|c| c.to_string()),
    }
}
