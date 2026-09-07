//! The capability set a UA advertises: the methods it accepts (`Allow`, RFC
//! 3261 §20.5), the option tags it understands (`Supported`, §20.37) and the
//! body types it takes (`Accept`, §20.1).
//!
//! Each half is a *value* — a token set, or a list of media ranges — that a
//! caller states and the generators render once at freeze. Each half is also
//! OPTIONAL: `None` carries no line, which §20.5 reads as "nothing stated",
//! and is distinct from a present-but-empty half, which renders a value-less
//! line and states the empty set. A face that carries a peer's advertisement
//! across a back-to-back UA states exactly what the peer stated
//! ([`CapabilitySet::relayed`]); [`CapabilitySet::default`] is the stack's own
//! set, for the messages the stack answers on its own behalf.
//!
//! The token sets are the grammar barrier: an entry that is not an RFC 3261
//! §25.1 `token` never enters one, so a set built from caller-supplied strings
//! cannot render a second value or inject a header line.

use crate::draft::Entry;
use crate::parser::custom::structured_headers::top_level_comma_entries;
use crate::header::{AcceptRange, Allow, HeaderName, HeaderValue, Supported};
use crate::sip_str::SipStr;
use crate::types::SipHeader;

/// RFC 3261 §13.2.1 / §20.5 — the methods the stack accepts, on a message it
/// answers on its own behalf.
pub const B2BUA_ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, REFER, NOTIFY, PRACK";
/// RFC 3261 §20.37 — the option tags the stack understands, on a message it
/// answers on its own behalf. An option tag obliges whoever advertises it, so
/// a tag no part of this stack exercises is not a member: RFC 4028's `timer`
/// would promise a session refresh nothing here performs.
pub const B2BUA_SUPPORTED: &str = "100rel, replaces";
/// RFC 3261 §20.1 — the body types the stack takes, on a message it answers
/// on its own behalf.
pub const B2BUA_ACCEPT: &str = "application/sdp";

/// One face's advertised capabilities. Immutable; narrow with
/// [`without_option_tag`](Self::without_option_tag).
///
/// Per half, three statements: `None` carries no line; `Some` and empty
/// renders a value-less line (§20.5: "accepts no methods"; §20.37: no option
/// tags understood); `Some` and populated states the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySet {
    allow: Option<Allow>,
    supported: Option<Supported>,
    accept: Option<Vec<AcceptRange>>,
}

impl CapabilitySet {
    /// The set stating exactly `allow` and `supported`, and no `Accept`.
    pub fn new(allow: Allow, supported: Supported) -> Self {
        Self { allow: Some(allow), supported: Some(supported), accept: None }
    }

    /// The set stating each half as given: `None` carries no line.
    pub fn stating(
        allow: Option<Allow>,
        supported: Option<Supported>,
        accept: Option<Vec<AcceptRange>>,
    ) -> Self {
        Self { allow, supported, accept }
    }

    /// The set that carries no line at all — what a face states when nothing
    /// was declared for it and nothing was received to relay.
    pub fn silent() -> Self {
        Self { allow: None, supported: None, accept: None }
    }

    /// The accepted-method set rendered into `Allow`, where stated.
    pub fn allow(&self) -> Option<&Allow> {
        self.allow.as_ref()
    }

    /// The option-tag set rendered into `Supported`, where stated.
    pub fn supported(&self) -> Option<&Supported> {
        self.supported.as_ref()
    }

    /// The media ranges rendered into `Accept`, where stated.
    pub fn accept(&self) -> Option<&[AcceptRange]> {
        self.accept.as_deref()
    }

    /// This set without option tag `tag` (case-insensitive), for a face that
    /// must not claim an extension it never exercised — e.g. `100rel` toward a
    /// peer that saw no reliable provisional from us. The other halves are
    /// untouched; a `Supported` this set does not state stays unstated.
    pub fn without_option_tag(&self, tag: &str) -> Self {
        Self {
            allow: self.allow.clone(),
            supported: self.supported.clone().map(|s| s.without(tag)),
            accept: self.accept.clone(),
        }
    }

    /// The set a face states when it carries the peer's own advertisement
    /// through (RFC 3261 §16.6): per half, exactly what `received` states —
    /// the received lines read as one set (§7.3.1) — and NO line where the
    /// peer carried none. An advertisement is a claim about the party that
    /// makes it, so a relay never widens, narrows or invents one.
    pub fn relayed(received: &[SipHeader]) -> Self {
        Self {
            allow: line_value::<Allow>(received),
            supported: line_value::<Supported>(received),
            accept: accept_value(received),
        }
    }

    /// The header lines this set states, `Allow` then `Supported` then
    /// `Accept`, as draft entries a rule can stamp on a message it mints. A
    /// half this set does not state contributes no entry.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::with_capacity(3);
        if let Some(allow) = &self.allow {
            out.push(Entry::typed(allow.clone()));
        }
        if let Some(supported) = &self.supported {
            out.push(Entry::typed(supported.clone()));
        }
        if let Some(text) = self.accept_text() {
            out.push(Entry::raw(HeaderName::Accept, SipStr::owned(&text)));
        }
        out
    }

    /// The `(name, value)` of every line this set states, in the order
    /// [`entries`](Self::entries) gives — for a mint point that stamps raw
    /// header lines.
    pub fn lines(&self) -> Vec<(HeaderName, String)> {
        let mut out = Vec::with_capacity(3);
        if let Some(text) = self.allow_text() {
            out.push((HeaderName::Allow, text));
        }
        if let Some(text) = self.supported_text() {
            out.push((HeaderName::Supported, text));
        }
        if let Some(text) = self.accept_text() {
            out.push((HeaderName::Accept, text));
        }
        out
    }

    /// The `Allow` header value as it reaches the wire, where stated.
    pub fn allow_text(&self) -> Option<String> {
        self.allow.as_ref().map(HeaderValue::to_wire)
    }

    /// The `Supported` header value as it reaches the wire, where stated.
    pub fn supported_text(&self) -> Option<String> {
        self.supported.as_ref().map(HeaderValue::to_wire)
    }

    /// The `Accept` header value as it reaches the wire, where stated: the
    /// media ranges comma-separated on one line (RFC 3261 §7.3.1).
    pub fn accept_text(&self) -> Option<String> {
        self.accept.as_ref().map(|ranges| {
            ranges.iter().map(HeaderValue::to_wire).collect::<Vec<_>>().join(", ")
        })
    }
}

/// The value `received` states for the set-like header `V`, or `None` when it
/// carries no line of that header. Several lines are one set (RFC 3261 §7.3.1),
/// and a line whose value is empty states the empty set — a statement, distinct
/// from carrying no line at all.
fn line_value<V: HeaderValue>(received: &[SipHeader]) -> Option<V> {
    let name = V::header_name();
    let lines: Vec<V> = received
        .iter()
        .filter(|h| name.matches(&h.name))
        .filter_map(|h| V::parse(&h.value).ok())
        .collect();
    V::combine(lines)
}

/// The media ranges `received` states across its `Accept` lines, in order,
/// or `None` when it carries no such line. Each comma-separated entry reads on
/// its own (RFC 3261 §7.3.1): one that is not a media range is dropped and the
/// rest stand, a repeat is stated once, and a line stating none contributes
/// none.
fn accept_value(received: &[SipHeader]) -> Option<Vec<AcceptRange>> {
    let name = HeaderName::Accept;
    let mut seen = false;
    let mut ranges: Vec<AcceptRange> = Vec::new();
    for line in received.iter().filter(|h| name.matches(&h.name)) {
        seen = true;
        for entry in top_level_comma_entries(line.value.as_str()) {
            let Ok(range) = AcceptRange::parse(&line.value.reslice(entry)) else { continue };
            if !ranges.iter().any(|r| r.to_wire().eq_ignore_ascii_case(&range.to_wire())) {
                ranges.push(range);
            }
        }
    }
    seen.then_some(ranges)
}

/// The stack's own capability set — what a message the stack answers on its
/// own behalf (an out-of-dialog OPTIONS) advertises.
impl Default for CapabilitySet {
    fn default() -> Self {
        Self {
            allow: Some(Allow::of(B2BUA_ALLOW.split(',').map(str::trim))),
            supported: Some(Supported::of(B2BUA_SUPPORTED.split(',').map(str::trim))),
            accept: Some(vec![AcceptRange::new(B2BUA_ACCEPT)]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default set renders byte for byte as the advertised constants — the
    /// property that makes the node's own OPTIONS answer indistinguishable
    /// from today.
    #[test]
    fn default_set_renders_as_the_advertised_constants() {
        let caps = CapabilitySet::default();
        assert_eq!(caps.allow_text().as_deref(), Some(B2BUA_ALLOW));
        assert_eq!(caps.supported_text().as_deref(), Some(B2BUA_SUPPORTED));
        assert_eq!(caps.accept_text().as_deref(), Some(B2BUA_ACCEPT));
    }

    #[test]
    fn a_declared_set_renders_exactly_its_tokens() {
        let caps = CapabilitySet::new(
            Allow::of(["INVITE", "ACK", "CANCEL", "BYE"]),
            Supported::of(["timer"]),
        );
        assert_eq!(caps.allow_text().as_deref(), Some("INVITE, ACK, CANCEL, BYE"));
        assert_eq!(caps.supported_text().as_deref(), Some("timer"));
        assert_eq!(caps.accept_text(), None);
    }

    #[test]
    fn narrowing_drops_the_option_tag_case_insensitively_and_keeps_allow() {
        let caps = CapabilitySet::default().without_option_tag("100REL");
        assert_eq!(caps.supported_text().as_deref(), Some("replaces"));
        assert_eq!(caps.allow_text().as_deref(), Some(B2BUA_ALLOW));
        assert_eq!(CapabilitySet::silent().without_option_tag("100rel"), CapabilitySet::silent());
    }

    /// The set is the barrier: a declared token carrying CRLF is dropped rather
    /// than rendered, so it cannot become a header line of its own.
    #[test]
    fn a_declared_token_that_is_not_a_token_never_renders() {
        let caps = CapabilitySet::new(
            Allow::of(["INVITE", "ACK\r\nX-Evil: injected"]),
            Supported::of(["timer;q=1"]),
        );
        assert_eq!(caps.allow_text().as_deref(), Some("INVITE"));
        assert_eq!(caps.supported_text().as_deref(), Some(""));
    }

    /// An empty half advertises the empty set — a value-less line, not the
    /// stack default and not an omitted header — and a `None` half omits it.
    #[test]
    fn an_empty_half_renders_as_no_tokens_and_an_absent_half_renders_nothing() {
        let caps = CapabilitySet::new(Allow::empty(), Supported::of(["timer"]));
        assert_eq!(caps.allow_text().as_deref(), Some(""));
        assert_eq!(caps.supported_text().as_deref(), Some("timer"));
        assert_eq!(caps.entries().len(), 2);
        assert!(CapabilitySet::silent().entries().is_empty());
        assert!(CapabilitySet::silent().lines().is_empty());
    }

    fn received(lines: &[(&str, &str)]) -> Vec<SipHeader> {
        lines
            .iter()
            .map(|(name, value)| SipHeader {
                name: crate::sip_str::SipStr::owned(name),
                value: crate::sip_str::SipStr::owned(value),
            })
            .collect()
    }

    /// A relayed set is the peer's, token for token: nothing of the stack's own
    /// is added to the methods, and an extension is claimed only where the
    /// peer claimed it.
    #[test]
    fn a_relayed_set_is_the_peer_tokens_and_nothing_more() {
        let caps = CapabilitySet::relayed(&received(&[
            ("Allow", "INVITE, ACK, BYE, MESSAGE"),
            ("Supported", "path"),
            ("Accept", "application/sdp, application/isup, application/xml"),
        ]));
        assert_eq!(caps.allow_text().as_deref(), Some("INVITE, ACK, BYE, MESSAGE"));
        assert_eq!(caps.supported_text().as_deref(), Some("path"));
        assert_eq!(
            caps.accept_text().as_deref(),
            Some("application/sdp, application/isup, application/xml")
        );
    }

    /// A half the peer never advertised is not stated: silence is relayed as
    /// silence, never as the stack's own set.
    #[test]
    fn a_half_the_peer_never_sent_carries_no_line() {
        let caps = CapabilitySet::relayed(&received(&[("Supported", "100rel")]));
        assert_eq!(caps.allow(), None);
        assert_eq!(caps.supported_text().as_deref(), Some("100rel"));
        assert_eq!(caps.accept(), None);
        assert_eq!(CapabilitySet::relayed(&[]), CapabilitySet::silent());
    }

    /// Compact and repeated lines are the same set (RFC 3261 §7.3.1/§7.3.3),
    /// and an empty line states the empty set rather than nothing.
    #[test]
    fn repeated_and_empty_lines_read_as_one_set() {
        let caps = CapabilitySet::relayed(&received(&[("Supported", "timer"), ("k", "replaces")]));
        assert_eq!(caps.supported_text().as_deref(), Some("timer, replaces"));

        let none = CapabilitySet::relayed(&received(&[("Supported", "")]));
        assert_eq!(none.supported_text().as_deref(), Some(""));
    }

    /// Several `Accept` lines are one list, a space-free spelling reads the
    /// same ranges, a repeated range is stated once, and an entry that is not
    /// a media range (a trailing comma) drops alone, never the line.
    #[test]
    fn accept_lines_read_as_one_list_of_media_ranges() {
        let caps = CapabilitySet::relayed(&received(&[
            ("Accept", "application/sdp,application/isup,"),
            ("Accept", "application/xml;q=0.5, application/sdp"),
        ]));
        assert_eq!(
            caps.accept_text().as_deref(),
            Some("application/sdp, application/isup, application/xml;q=0.5")
        );
        let empty = CapabilitySet::relayed(&received(&[("Accept", "")]));
        assert_eq!(empty.accept_text().as_deref(), Some(""));
    }

    #[test]
    fn entries_carry_allow_then_supported_then_accept() {
        let caps = CapabilitySet::default();
        let entries = caps.entries();
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is(&HeaderName::Allow));
        assert!(entries[1].is(&HeaderName::Supported));
        assert!(entries[2].is(&HeaderName::Accept));
        assert_eq!(entries[0].text().as_str(), B2BUA_ALLOW);
        assert_eq!(entries[1].text().as_str(), B2BUA_SUPPORTED);
        assert_eq!(entries[2].text().as_str(), B2BUA_ACCEPT);
    }
}
