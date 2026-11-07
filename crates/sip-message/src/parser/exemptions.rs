//! The shapes a deployment accepts against the RFC: empty by default, each one
//! stated at startup, consulted only by the ingress gates (ADR-0001). Only a
//! shape with one reading can be exempted, and every typed reader accepts it on
//! every deployment, so a read of stored bytes needs no configuration.

use std::borrow::Cow;

/// One shape a deployment may state. A name no variant holds refuses the boot:
/// a typo never reads as a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exemption {}

impl Exemption {
    /// The exemption `name` states, `None` when no exemption has that name.
    fn named(_name: &str) -> Option<Self> {
        None
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
        let named = stated.split(',').map(str::trim).map(|name| match name {
            "" => Err(format!("names nothing in {stated:?}")),
            name => Exemption::named(name).ok_or_else(|| format!("names no exemption: {name:?}")),
        });
        named.collect::<Result<Vec<_>, _>>().map(|set| Exemptions(Cow::Owned(set)))
    }

    /// Whether the set holds `exemption`.
    pub fn holds(&self, exemption: Exemption) -> bool {
        self.0.contains(&exemption)
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
    fn a_name_no_exemption_has_is_refused_naming_it() {
        let refusal = Exemptions::read(Some("not-an-exemption")).expect_err("refused");
        assert!(refusal.contains("not-an-exemption"), "{refusal}");
    }
}
