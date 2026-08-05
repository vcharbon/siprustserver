//! [`TokenListHeader`] — the set-like headers (Require, Supported, Allow, …).
//!
//! The value is the option-tag *set*, so a reader asks `contains("100rel")`
//! instead of splitting a string; several lines of the same header union into
//! one set, which is the semantics RFC 3261 §7.3.1 gives them. The family is
//! declared [`Folding::SetPerLine`]: the separators on a line belong to this
//! grammar, which reads them into one set, not to the line-splitting rule.
//! Which separator that is comes from the header's own grammar
//! ([`HeaderName::item_separator`]) — a comma for the option-tag headers, a
//! `;` for the RFC 3323 §4.2 priv-value set.

use std::marker::PhantomData;

use crate::error::SipParseError;
use crate::parser::custom::scanner::is_token_char;
use crate::sip_str::SipStr;

use super::kind::TokenKind;
use super::name::HeaderName;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// Whether `s` is one RFC 3261 §25.1 `token`: at least one character, all of
/// them token characters.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_token_char)
}

/// An ordered set of tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenListHeader<K: TokenKind> {
    tokens: Vec<SipStr>,
    kind: PhantomData<fn() -> K>,
}

impl<K: TokenKind> TokenListHeader<K> {
    pub fn empty() -> Self {
        Self { tokens: Vec::new(), kind: PhantomData }
    }

    pub fn of(tokens: impl IntoIterator<Item = impl Into<SipStr>>) -> Self {
        let mut set = Self::empty();
        for token in tokens {
            set = set.with(token);
        }
        set
    }

    /// Whether `token` is in the set, case-insensitively — option tags are
    /// case-insensitive (RFC 3261 §7.3.1).
    pub fn contains(&self, token: &str) -> bool {
        self.tokens.iter().any(|t| t.eq_ignore_ascii_case(token))
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.tokens.iter().map(SipStr::as_str)
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Add `token` if it is a well-formed [`token`](is_token) the set does not
    /// already carry. Anything else — a comma list, an embedded space, a value
    /// carrying CR/LF — is DROPPED: the set is the stack's grammar barrier, so
    /// a caller-supplied string can never render a second value or a second
    /// header line.
    pub fn with(mut self, token: impl Into<SipStr>) -> Self {
        let token: SipStr = token.into();
        if is_token(token.as_str()) && !self.contains(&token) {
            self.tokens.push(token);
        }
        self
    }

    pub fn without(mut self, token: &str) -> Self {
        self.tokens.retain(|t| !t.eq_ignore_ascii_case(token));
        self
    }
}

impl<K: TokenKind> HeaderValue for TokenListHeader<K> {
    fn header_name() -> HeaderName {
        K::name()
    }

    fn folding() -> Folding {
        K::FOLDING
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let mut set = Self::empty();
        for entry in K::name().item_separator().split(raw.as_str()) {
            set = set.with(raw.reslice(entry));
        }
        Ok(set)
    }

    fn render(&self, out: &mut Wire) {
        let joiner = K::name().item_separator().joiner();
        for (i, token) in self.tokens.iter().enumerate() {
            if i > 0 {
                out.str(joiner);
            }
            out.str(token.as_str());
        }
    }

    /// Several lines of a set-like header are one set.
    fn combine(values: Vec<Self>) -> Option<Self> {
        let mut it = values.into_iter();
        let mut merged = it.next()?;
        for extra in it {
            for token in extra.tokens {
                merged = merged.with(token);
            }
        }
        Some(merged)
    }
}

impl<K: TokenKind> std::fmt::Display for TokenListHeader<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{Privacy, Require, Supported};

    #[test]
    fn option_tags_match_case_insensitively() {
        let r = Require::parse(&SipStr::owned("100rel, timer")).unwrap();
        assert!(r.contains("100REL"));
        assert!(r.contains("timer"));
        assert!(!r.contains("replaces"));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn several_lines_union_into_one_set() {
        let lines = vec![
            Supported::parse(&SipStr::owned("100rel")).unwrap(),
            Supported::parse(&SipStr::owned("timer, 100rel")).unwrap(),
        ];
        let merged = Supported::combine(lines).unwrap();
        assert_eq!(merged.to_wire(), "100rel, timer");
    }

    #[test]
    fn a_value_that_is_not_a_token_never_enters_the_set() {
        let set = Supported::empty()
            .with("timer")
            .with("evil\r\nX-Injected: yes")
            .with("two words")
            .with("100rel, replaces")
            .with("");
        assert_eq!(set.to_wire(), "timer");
    }

    /// RFC 3323 §4.2: the priv-values are `;`-separated, so the set reads and
    /// writes them that way while the option-tag family keeps its commas.
    #[test]
    fn the_priv_value_set_reads_and_writes_semicolons() {
        let p = Privacy::parse(&SipStr::owned("id;user; critical")).unwrap();
        assert!(p.contains("id"));
        assert!(p.contains("USER"));
        assert_eq!(p.len(), 3);
        assert_eq!(p.to_wire(), "id;user;critical");
        assert_eq!(Supported::of(["100rel", "timer"]).to_wire(), "100rel, timer");
    }

    /// A comma is DATA inside a priv-value list: splitting on it would mint a
    /// token the sender never wrote.
    #[test]
    fn a_comma_is_not_a_priv_value_separator() {
        let p = Privacy::parse(&SipStr::owned("id,user")).unwrap();
        assert_eq!(p.len(), 0, "`id,user` is one malformed priv-value, not two: {p:?}");
    }

    #[test]
    fn adding_a_present_tag_is_a_no_op() {
        let r = Require::empty().with("100rel").with("100REL");
        assert_eq!(r.len(), 1);
        assert_eq!(r.without("100rel").len(), 0);
    }
}
