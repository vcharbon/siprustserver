//! The structural/end-to-end classification of a header name — the crate's
//! single answer to "does the stack own this header, or does it belong to the
//! peers?".
//!
//! Distinct from [`crate::template::HeaderClass`], which answers the narrower
//! template question (regenerate vs emit byte-for-byte) and is expressed over
//! this table.

use super::name::HeaderName;

/// Who owns a header on a hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderClass {
    /// Hop-scoped: routing, transaction and dialog identity, plus the
    /// serializer-owned Content-Length. A stack that emits a message writes
    /// these from its own state (RFC 3261 §16.6) — it never copies a peer's.
    Structural,
    /// End-to-end: meaningful to the far end, relayed unchanged.
    EndToEnd,
}

impl HeaderName {
    /// This header's owner.
    pub fn class(&self) -> HeaderClass {
        match self {
            HeaderName::Via
            | HeaderName::From
            | HeaderName::To
            | HeaderName::CallId
            | HeaderName::CSeq
            | HeaderName::Contact
            | HeaderName::Route
            | HeaderName::RecordRoute
            | HeaderName::MaxForwards
            | HeaderName::ContentLength => HeaderClass::Structural,
            _ => HeaderClass::EndToEnd,
        }
    }

    /// The class of a wire name (any casing, compact or long form) without
    /// minting a [`HeaderName`] for an extension header — extensions are always
    /// end-to-end.
    pub fn class_of(name: &str) -> HeaderClass {
        Self::known(name).map_or(HeaderClass::EndToEnd, |known| known.class())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_owned_set_is_structural() {
        for name in ["Via", "From", "To", "Call-ID", "CSeq", "Contact", "Route", "Record-Route",
                     "Max-Forwards", "Content-Length"] {
            assert_eq!(HeaderName::class_of(name), HeaderClass::Structural, "{name}");
        }
    }

    #[test]
    fn everything_else_is_end_to_end() {
        for name in ["Content-Type", "P-Asserted-Identity", "Supported", "Allow", "X-Custom"] {
            assert_eq!(HeaderName::class_of(name), HeaderClass::EndToEnd, "{name}");
        }
    }

    #[test]
    fn compact_forms_classify_as_their_long_name() {
        assert_eq!(HeaderName::class_of("v"), HeaderClass::Structural);
        assert_eq!(HeaderName::class_of("l"), HeaderClass::Structural);
        assert_eq!(HeaderName::class_of("c"), HeaderClass::EndToEnd, "c = Content-Type");
    }
}
