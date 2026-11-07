//! The shapes a deployment accepts against the RFC: empty by default, each one
//! stated at startup, consulted only by the ingress gates (ADR-0001). Only a
//! shape with one reading can be exempted, and every typed reader accepts it on
//! every deployment, so a read of stored bytes needs no configuration.

use std::borrow::Cow;

use crate::char_class::is_paramchar;
use crate::header::HeaderName;

/// One shape a deployment may state. A name no variant holds refuses the boot:
/// a typo never reads as a setting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Exemption {
    /// `repeat:<header>.uri:<param>`: the URI of `header` written with the
    /// parameter `param` twice, the values equal as RFC 3261 §19.1.4 compares
    /// them (§19.1.1 allows it once); the first copy is the one read and
    /// written. A second value is refused whatever is named.
    RepeatedUriParam { header: HeaderName, param: String },
    /// `bare-user-semicolon`: a From or To written without brackets whose
    /// URI holds a `;` before its `@`, which RFC 3261 §20 brackets.
    BareUserSemicolon,
    /// `max-forwards:absent`: a request with no Max-Forwards (§8.1.1), read
    /// as 70 (§16.6).
    MaxForwardsAbsent,
}

/// The headers whose URI a `repeat:<header>.uri:<param>` may name: those
/// ingress reads an address of.
const URI_HEADERS: &[HeaderName] = &[
    HeaderName::From,
    HeaderName::To,
    HeaderName::Contact,
    HeaderName::Route,
    HeaderName::RecordRoute,
    HeaderName::ReferTo,
];

impl Exemption {
    /// The exemption `name` states, `None` when no exemption has that name.
    fn named(name: &str) -> Option<Self> {
        match name {
            "bare-user-semicolon" => return Some(Self::BareUserSemicolon),
            "max-forwards:absent" => return Some(Self::MaxForwardsAbsent),
            _ => {}
        }
        let (header, param) = name.strip_prefix("repeat:")?.split_once(".uri:")?;
        let header = HeaderName::known(header).filter(|h| URI_HEADERS.contains(h))?;
        let token = !param.is_empty() && param.bytes().all(is_paramchar);
        token.then(|| Self::RepeatedUriParam { header, param: param.to_ascii_lowercase() })
    }
}

/// A set of [`Exemption`]s, built without allocating when empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exemptions(Cow<'static, [Exemption]>);

impl Exemptions {
    /// No exemption: every shape is held to the RFC.
    pub const NONE: Exemptions = Exemptions(Cow::Borrowed(&[]));

    /// The set a variable states: none when unset. A value set blank, a blank
    /// entry, and a name no exemption has are each refused, on every tier alike:
    /// a typo never reads as a setting (ADR-0046).
    pub fn read(stated: Option<&str>) -> Result<Self, String> {
        let Some(stated) = stated else { return Ok(Self::NONE) };
        let named = stated.split(',').map(crate::whitespace::trim).map(|name| match name {
            "" => Err(format!("names nothing in {stated:?}")),
            name => Exemption::named(name).ok_or_else(|| format!("names no exemption: {name:?}")),
        });
        named.collect::<Result<Vec<_>, _>>().map(|set| Exemptions(Cow::Owned(set)))
    }

    /// Whether the set holds `exemption`.
    pub fn holds(&self, exemption: &Exemption) -> bool {
        self.0.contains(exemption)
    }

    /// Whether the set lets the URI of `header` carry `param` twice with
    /// equal values.
    pub fn repeats(&self, header: &HeaderName, param: &str) -> bool {
        self.0.iter().any(|e| {
            matches!(e, Exemption::RepeatedUriParam { header: h, param: p }
                if h == header && p.eq_ignore_ascii_case(param))
        })
    }

    /// Whether the set holds nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Default for Exemptions {
    fn default() -> Self {
        Self::NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_states_nothing() {
        assert!(Exemptions::read(None).expect("nothing to refuse").is_empty());
    }

    #[test]
    fn a_blank_value_or_entry_is_refused() {
        for stated in ["", "  ", ",", " , "] {
            assert!(Exemptions::read(Some(stated)).is_err(), "{stated:?}");
        }
    }

    #[test]
    fn every_name_reads_as_its_exemption() {
        let set = Exemptions::read(Some(
            "repeat:from.uri:x-dup, repeat:Contact.uri:x-two,bare-user-semicolon,max-forwards:absent",
        ))
        .expect("each name reads");
        assert!(set.repeats(&HeaderName::From, "X-DUP"));
        assert!(set.repeats(&HeaderName::Contact, "x-two"));
        assert!(!set.repeats(&HeaderName::To, "x-dup"));
        assert!(set.holds(&Exemption::BareUserSemicolon));
        assert!(set.holds(&Exemption::MaxForwardsAbsent));
    }

    /// A repeat names a header ingress reads an address of, and a parameter.
    #[test]
    fn a_repeat_of_no_address_header_or_no_parameter_is_refused() {
        for stated in
            ["repeat:via.uri:x", "repeat:from.uri:", "repeat:from:x", "repeat:from.uri:a b"]
        {
            assert!(Exemptions::read(Some(stated)).is_err(), "{stated}");
        }
    }

    #[test]
    fn a_name_no_exemption_has_is_refused_naming_it() {
        let refusal = Exemptions::read(Some("not-an-exemption")).expect_err("refused");
        assert!(refusal.contains("not-an-exemption"), "{refusal}");
    }
}
