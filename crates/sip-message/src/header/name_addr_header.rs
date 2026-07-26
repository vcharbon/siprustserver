//! [`NameAddrHeader`] — one implementation of the address-header family, made
//! nominal by its kind.
//!
//! `NameAddrHeader<kind::From>` and `NameAddrHeader<kind::To>` are different
//! types that cannot be swapped, share every accessor, and differ in surface
//! exactly where the grammars differ: the tag API exists only on the tagged
//! kinds, the parameter API only on the kinds whose grammar has parameters.

use std::marker::PhantomData;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::kind::{NameAddrKind, RichParams, TaggedKind};
use super::name::HeaderName;
use super::name_addr::NameAddr;
use super::params::{ParamValue, Params};
use super::uri::Uri;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// An address-valued header. `K` carries the identity and the policy; the
/// address itself is a plain [`NameAddr`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameAddrHeader<K: NameAddrKind> {
    addr: NameAddr,
    kind: PhantomData<fn() -> K>,
}

impl<K: NameAddrKind> NameAddrHeader<K> {
    pub fn new(addr: NameAddr) -> Self {
        Self { addr, kind: PhantomData }
    }

    /// The bare-URI form — the shape a stack builds when it has no display name.
    pub fn from_uri(uri: Uri) -> Self {
        Self::new(NameAddr::new(uri))
    }

    pub fn addr(&self) -> &NameAddr {
        &self.addr
    }

    pub fn into_addr(self) -> NameAddr {
        self.addr
    }

    pub fn uri(&self) -> &Uri {
        self.addr.uri()
    }

    pub fn display(&self) -> Option<&str> {
        self.addr.display()
    }

    pub fn with_uri(self, uri: Uri) -> Self {
        Self::new(self.addr.with_uri(uri))
    }

    pub fn with_display(self, display: impl Into<SipStr>) -> Self {
        Self::new(self.addr.with_display(display))
    }

    pub fn without_display(self) -> Self {
        Self::new(self.addr.without_display())
    }

    /// Re-interpret this address under another kind — the one conversion the
    /// dialog rules need (a To becomes the next request's From, a Contact
    /// becomes a Route entry).
    pub fn retarget<J: NameAddrKind>(self) -> NameAddrHeader<J> {
        NameAddrHeader::new(self.addr)
    }
}

impl<K: RichParams> NameAddrHeader<K> {
    pub fn params(&self) -> &Params {
        self.addr.params()
    }

    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.addr.params().get(name)
    }

    pub fn with_param(self, name: impl Into<SipStr>, value: ParamValue) -> Self {
        Self::new(self.addr.with_param(name, value))
    }

    pub fn with_flag(self, name: impl Into<SipStr>) -> Self {
        Self::new(self.addr.with_param(name, ParamValue::Flag))
    }

    pub fn without_param(self, name: &str) -> Self {
        Self::new(self.addr.without_param(name))
    }
}

impl<K: TaggedKind> NameAddrHeader<K> {
    /// The dialog tag, when the peer has assigned one.
    pub fn tag(&self) -> Option<&str> {
        self.addr.params().value("tag")
    }

    pub fn with_tag(self, tag: impl Into<SipStr>) -> Self {
        Self::new(self.addr.with_param(SipStr::from_static("tag"), ParamValue::Token(tag.into())))
    }

    pub fn without_tag(self) -> Self {
        Self::new(self.addr.without_param("tag"))
    }
}

impl<K: NameAddrKind> HeaderValue for NameAddrHeader<K> {
    fn header_name() -> HeaderName {
        K::name()
    }

    fn folding() -> Folding {
        K::FOLDING
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        NameAddr::parse(raw).map(Self::new)
    }

    fn render(&self, out: &mut Wire) {
        self.addr.render(out)
    }
}

impl<K: NameAddrKind> std::fmt::Display for NameAddrHeader<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut w = Wire::new();
        self.addr.render(&mut w);
        f.write_str(w.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{Contact, To};

    #[test]
    fn the_tag_api_reads_and_rewrites_in_place() {
        let to = To::parse(&SipStr::owned("<sip:bob@biloxi.com>")).unwrap();
        assert_eq!(to.tag(), None);
        let tagged = to.with_tag("a6c85cf");
        assert_eq!(tagged.tag(), Some("a6c85cf"));
        assert_eq!(tagged.to_string(), "<sip:bob@biloxi.com>;tag=a6c85cf");
        assert_eq!(tagged.without_tag().to_string(), "<sip:bob@biloxi.com>");
    }

    #[test]
    fn a_contact_reads_its_own_parameters() {
        let c = Contact::parse(&SipStr::owned("<sip:bob@1.2.3.4:5070>;q=0.5;expires=300")).unwrap();
        assert_eq!(c.uri().host_port(), ("1.2.3.4", 5070));
        assert_eq!(c.param("q").and_then(ParamValue::as_str), Some("0.5"));
        assert_eq!(c.param("EXPIRES").and_then(ParamValue::as_str), Some("300"));
    }

    #[test]
    fn kinds_declare_their_own_identity_and_folding() {
        assert_eq!(To::header_name(), HeaderName::To);
        assert_eq!(To::folding(), Folding::Single);
        assert_eq!(Contact::header_name(), HeaderName::Contact);
        assert_eq!(Contact::folding(), Folding::Comma);
    }
}
