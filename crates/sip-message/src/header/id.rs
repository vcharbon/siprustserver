//! Resolving a wire name to its [`HeaderId`] in a bounded number of byte
//! compares, and [`HeaderKey`], the form a header lookup compares by.
//!
//! The known names are bucketed by length at compile time; a lookup reads the
//! one bucket its name's length selects (at most eight names) and compares a
//! candidate only when its first letter matches. An extension name costs the
//! same bounded probe as a known one.

use super::name::HeaderId;
use crate::parser::custom::compact_forms::expanded_name;

/// One past the longest known wire name.
const LENGTHS: usize = 26;
/// The most known names sharing one length.
const BUCKET: usize = 8;

/// The known ids by the length of their wire name.
const BY_LENGTH: [[Option<HeaderId>; BUCKET]; LENGTHS] = [
    bucket(0),
    bucket(1),
    bucket(2),
    bucket(3),
    bucket(4),
    bucket(5),
    bucket(6),
    bucket(7),
    bucket(8),
    bucket(9),
    bucket(10),
    bucket(11),
    bucket(12),
    bucket(13),
    bucket(14),
    bucket(15),
    bucket(16),
    bucket(17),
    bucket(18),
    bucket(19),
    bucket(20),
    bucket(21),
    bucket(22),
    bucket(23),
    bucket(24),
    bucket(25),
];

/// The known ids whose wire name is `len` bytes long. Every known id finds its
/// slot: the resolution test reads each one back.
const fn bucket(len: usize) -> [Option<HeaderId>; BUCKET] {
    let mut out = [None; BUCKET];
    let mut rest = HeaderId::ALL;
    while let [id, tail @ ..] = rest {
        if id.wire().len() == len {
            let mut slots: &mut [Option<HeaderId>] = &mut out;
            loop {
                match slots {
                    [] => break,
                    [slot, more @ ..] => {
                        if slot.is_none() {
                            *slot = Some(*id);
                            break;
                        }
                        slots = more;
                    }
                }
            }
        }
        rest = tail;
    }
    out
}

impl HeaderId {
    /// The known header `name` spells — any casing, long or compact form
    /// (RFC 3261 §7.3.3) — or `None` for an extension header.
    pub fn of(name: &str) -> Option<HeaderId> {
        let wire = expanded_name(name).as_bytes();
        let first = wire.first()?.to_ascii_lowercase();
        BY_LENGTH.get(wire.len())?.iter().flatten().copied().find(|id| {
            let candidate = id.wire().as_bytes();
            candidate.first().map(u8::to_ascii_lowercase) == Some(first)
                && candidate.eq_ignore_ascii_case(wire)
        })
    }

    /// This id's bit in a presence set of every known header.
    pub const fn bit(self) -> u128 {
        1u128 << (self as u8)
    }
}

/// What a header lookup compares by: a known header's id, or an extension
/// header's name, matched case-insensitively.
#[derive(Debug, Clone, Copy)]
pub enum HeaderKey<'a> {
    Known(HeaderId),
    Extension(&'a str),
}

impl HeaderKey<'_> {
    /// The known header this key names; `None` for an extension header.
    pub fn id(&self) -> Option<HeaderId> {
        match self {
            HeaderKey::Known(id) => Some(*id),
            HeaderKey::Extension(_) => None,
        }
    }

    /// Whether a line classified as `id`, named `wire`, carries this header.
    pub fn is(&self, id: Option<HeaderId>, wire: &str) -> bool {
        match (self, id) {
            (HeaderKey::Known(want), Some(id)) => *want == id,
            (HeaderKey::Extension(want), None) => want.eq_ignore_ascii_case(wire),
            _ => false,
        }
    }

    /// Whether `wire`, a header name as written, carries this header.
    pub fn matches(&self, wire: &str) -> bool {
        self.is(HeaderId::of(wire), wire)
    }
}

impl PartialEq for HeaderKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HeaderKey::Known(a), HeaderKey::Known(b)) => a == b,
            (HeaderKey::Extension(a), HeaderKey::Extension(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }
}

impl Eq for HeaderKey<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::HeaderName;

    #[test]
    fn every_known_id_resolves_from_its_wire_spelling_in_any_casing() {
        for id in HeaderId::ALL {
            assert_eq!(HeaderId::of(id.wire()), Some(*id));
            assert_eq!(HeaderId::of(&id.wire().to_ascii_uppercase()), Some(*id));
            assert_eq!(id.name().id(), Some(*id));
        }
    }

    #[test]
    fn every_id_has_its_own_presence_bit() {
        let all = HeaderId::ALL.iter().fold(0u128, |set, id| {
            assert_eq!(set & id.bit(), 0, "{id:?} shares a bit");
            set | id.bit()
        });
        assert_eq!(all.count_ones() as usize, HeaderId::ALL.len());
    }

    #[test]
    fn extension_names_and_near_misses_resolve_to_nothing() {
        for name in ["", "X-Overload", "Vias", "Fro", "Contact-", "P-Asserted-Identit"] {
            assert_eq!(HeaderId::of(name), None, "{name}");
        }
    }

    #[test]
    fn an_other_name_spelling_a_known_header_keys_as_that_header() {
        let compact = HeaderName::Other("k".into());
        assert_eq!(compact.key(), HeaderKey::Known(HeaderId::Supported));
        let ext = HeaderName::Other("X-Overload".into());
        assert_eq!(ext.key(), HeaderName::Other("x-overload".into()).key());
        assert!(ext.key().matches("X-OVERLOAD"));
        assert!(!ext.key().matches("Supported"));
    }
}
