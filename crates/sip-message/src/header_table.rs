//! [`HeaderTable`] — a message's header lines, each classified once.
//!
//! Every line carries the [`HeaderId`] it was classified as when the message
//! was parsed or rendered, and the table keeps the set of known headers it
//! holds. A lookup compares one byte per line instead of a name, and a lookup
//! of a known header the message does not carry reads no line at all.

use std::borrow::Borrow;

use smallvec::SmallVec;

use crate::header::{HeaderId, HeaderKey, HeaderName};
use crate::types::SipHeader;

/// The ids of a message's lines, inline for the header counts the wire
/// usually carries.
pub(crate) type Ids = SmallVec<[Option<HeaderId>; 64]>;

/// Header lines in wire order, each with its classification.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderTable {
    lines: Vec<SipHeader>,
    ids: Ids,
    /// One bit per known header present ([`HeaderId::bit`]).
    present: u128,
}

impl HeaderTable {
    /// The table of `lines`, each classified here.
    pub fn new(lines: Vec<SipHeader>) -> Self {
        let ids = lines.iter().map(|line| HeaderId::of(&line.name)).collect();
        Self::classified(lines, ids)
    }

    /// The table of `lines` already classified as `ids`, one per line.
    pub(crate) fn classified(lines: Vec<SipHeader>, ids: Ids) -> Self {
        debug_assert_eq!(lines.len(), ids.len());
        let present = ids.iter().flatten().fold(0, |set, id| set | id.bit());
        Self { lines, ids, present }
    }

    /// Every line, in wire order.
    pub fn lines(&self) -> &[SipHeader] {
        &self.lines
    }

    /// The lines, giving up their classification.
    pub fn into_lines(self) -> Vec<SipHeader> {
        self.lines
    }

    /// Every line with its classification, in wire order.
    pub fn classified_lines(&self) -> impl Iterator<Item = (Option<HeaderId>, &SipHeader)> {
        self.ids.iter().copied().zip(&self.lines)
    }

    /// Whether the message carries `id` on any line.
    pub fn carries(&self, id: HeaderId) -> bool {
        self.present & id.bit() != 0
    }

    /// Whether the message carries the header `name` names.
    pub fn has(&self, name: &HeaderName) -> bool {
        self.of(name).next().is_some()
    }

    /// The lines of the header `name` names, in wire order. A known header
    /// the message does not carry reads no line.
    pub fn of<'a, N: Borrow<HeaderName> + 'a>(
        &'a self,
        name: N,
    ) -> impl Iterator<Item = &'a SipHeader> + 'a {
        let known = match name.borrow().key() {
            HeaderKey::Known(id) => Some(id),
            HeaderKey::Extension(_) => None,
        };
        let absent = known.is_some_and(|id| !self.carries(id));
        let lines: &[SipHeader] = if absent { &[] } else { self.lines.as_slice() };
        self.ids
            .iter()
            .copied()
            .zip(lines)
            .filter(move |(id, line)| match known {
                Some(want) => *id == Some(want),
                None => {
                    id.is_none() && name.borrow().as_wire_str().eq_ignore_ascii_case(&line.name)
                }
            })
            .map(|(_, line)| line)
    }

    /// The lines of the known header `id`, in wire order.
    pub fn of_id(&self, id: HeaderId) -> impl Iterator<Item = &SipHeader> + '_ {
        let lines: &[SipHeader] = if self.carries(id) { self.lines.as_slice() } else { &[] };
        self.ids.iter().copied().zip(lines).filter(move |(i, _)| *i == Some(id)).map(|(_, l)| l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip_str::SipStr;

    fn table(lines: &[(&str, &str)]) -> HeaderTable {
        HeaderTable::new(
            lines.iter().map(|(n, v)| SipHeader::new(SipStr::owned(n), SipStr::owned(v))).collect(),
        )
    }

    #[test]
    fn a_lookup_finds_every_spelling_of_its_header_in_wire_order() {
        let t = table(&[("v", "a"), ("X-A", "1"), ("VIA", "b"), ("x-a", "2")]);
        let vias: Vec<&str> = t.of(&HeaderName::Via).map(|h| h.value.as_str()).collect();
        assert_eq!(vias, ["a", "b"]);
        let ext = HeaderName::Other(SipStr::owned("X-a"));
        let xs: Vec<&str> = t.of(&ext).map(|h| h.value.as_str()).collect();
        assert_eq!(xs, ["1", "2"]);
        assert!(t.carries(HeaderId::Via));
        assert!(!t.has(&HeaderName::Route));
    }

    #[test]
    fn an_other_name_spelling_a_known_header_finds_that_header() {
        let t = table(&[("Supported", "timer")]);
        assert!(t.has(&HeaderName::Other(SipStr::owned("k"))));
    }
}
