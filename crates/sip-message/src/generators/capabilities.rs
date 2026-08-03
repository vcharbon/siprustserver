//! The capability set a UA advertises: the methods it accepts (`Allow`, RFC
//! 3261 §20.5) and the option tags it understands (`Supported`, §20.37).
//!
//! The set is a *value* — two token sets, not two strings — so a caller states
//! it with [`Allow`]/[`Supported`] and the generators render it once at freeze.
//! [`CapabilitySet::default`] is the stack's own set, rendering byte for byte
//! as [`B2BUA_ALLOW`] / [`B2BUA_SUPPORTED`].
//!
//! The token sets are the grammar barrier: an entry that is not an RFC 3261
//! §25.1 `token` never enters one, so a set built from caller-supplied strings
//! cannot render a second value or inject a header line.

use crate::draft::Entry;
use crate::header::{Allow, HeaderValue, Supported};

/// RFC 3261 §13.2.1 / §20.5 — the methods the stack accepts, advertised when
/// the caller declares no set of its own.
pub const B2BUA_ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, REFER, NOTIFY, PRACK";
/// RFC 3261 §20.37 — the option tags the stack understands, advertised when the
/// caller declares no set of its own.
pub const B2BUA_SUPPORTED: &str = "100rel, timer, replaces";

/// One face's advertised capabilities: accepted methods + understood option
/// tags. Immutable; narrow with [`without_option_tag`](Self::without_option_tag).
///
/// An EMPTY half is a statement, not an omission: it renders as a value-less
/// header line, which RFC 3261 §20.5 reads as "accepts no methods" (§20.37, no
/// option tags understood) — distinct from omitting the header, which carries
/// no information. A caller that means "keep the stack's half" states that
/// half from [`CapabilitySet::default`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySet {
    allow: Allow,
    supported: Supported,
}

impl CapabilitySet {
    /// The set advertising exactly `allow` and `supported`.
    pub fn new(allow: Allow, supported: Supported) -> Self {
        Self { allow, supported }
    }

    /// The accepted-method set rendered into `Allow`.
    pub fn allow(&self) -> &Allow {
        &self.allow
    }

    /// The option-tag set rendered into `Supported`.
    pub fn supported(&self) -> &Supported {
        &self.supported
    }

    /// This set without option tag `tag` (case-insensitive), for a face that
    /// must not claim an extension it never exercised — e.g. `100rel` toward a
    /// peer that saw no reliable provisional from us. `Allow` is untouched.
    pub fn without_option_tag(&self, tag: &str) -> Self {
        Self { allow: self.allow.clone(), supported: self.supported.clone().without(tag) }
    }

    /// The two header lines this set advertises, `Allow` then `Supported`, as
    /// draft entries a rule can stamp on a message it mints.
    pub fn entries(&self) -> [Entry; 2] {
        [Entry::typed(self.allow.clone()), Entry::typed(self.supported.clone())]
    }

    /// The `Allow` header value as it reaches the wire.
    pub fn allow_text(&self) -> String {
        self.allow.to_wire()
    }

    /// The `Supported` header value as it reaches the wire.
    pub fn supported_text(&self) -> String {
        self.supported.to_wire()
    }
}

/// The stack's own capability set — what every face advertises when nothing
/// narrower is declared.
impl Default for CapabilitySet {
    fn default() -> Self {
        Self {
            allow: Allow::of(B2BUA_ALLOW.split(',').map(str::trim)),
            supported: Supported::of(B2BUA_SUPPORTED.split(',').map(str::trim)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default set renders byte for byte as the advertised constants — the
    /// property that makes an undeclared face indistinguishable from today.
    #[test]
    fn default_set_renders_as_the_advertised_constants() {
        let caps = CapabilitySet::default();
        assert_eq!(caps.allow_text(), B2BUA_ALLOW);
        assert_eq!(caps.supported_text(), B2BUA_SUPPORTED);
    }

    #[test]
    fn a_declared_set_renders_exactly_its_tokens() {
        let caps = CapabilitySet::new(
            Allow::of(["INVITE", "ACK", "CANCEL", "BYE"]),
            Supported::of(["timer"]),
        );
        assert_eq!(caps.allow_text(), "INVITE, ACK, CANCEL, BYE");
        assert_eq!(caps.supported_text(), "timer");
    }

    #[test]
    fn narrowing_drops_the_option_tag_case_insensitively_and_keeps_allow() {
        let caps = CapabilitySet::default().without_option_tag("100REL");
        assert_eq!(caps.supported_text(), "timer, replaces");
        assert_eq!(caps.allow_text(), B2BUA_ALLOW);
    }

    /// The set is the barrier: a declared token carrying CRLF is dropped rather
    /// than rendered, so it cannot become a header line of its own.
    #[test]
    fn a_declared_token_that_is_not_a_token_never_renders() {
        let caps = CapabilitySet::new(
            Allow::of(["INVITE", "ACK\r\nX-Evil: injected"]),
            Supported::of(["timer;q=1"]),
        );
        assert_eq!(caps.allow_text(), "INVITE");
        assert_eq!(caps.supported_text(), "");
    }

    /// An empty half advertises the empty set — a value-less line, not the
    /// stack default and not an omitted header.
    #[test]
    fn an_empty_half_renders_as_no_tokens() {
        let caps = CapabilitySet::new(Allow::empty(), Supported::of(["timer"]));
        assert_eq!(caps.allow_text(), "");
        assert_eq!(caps.supported_text(), "timer");
    }

    #[test]
    fn entries_carry_allow_then_supported() {
        let caps = CapabilitySet::default();
        let [allow, supported] = caps.entries();
        assert!(allow.is(&crate::header::HeaderName::Allow));
        assert!(supported.is(&crate::header::HeaderName::Supported));
        assert_eq!(allow.text().as_str(), B2BUA_ALLOW);
        assert_eq!(supported.text().as_str(), B2BUA_SUPPORTED);
    }
}
