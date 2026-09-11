//! The separator a header's own grammar puts between the ITEMS of one value —
//! the crate's single answer to "what splits this header's list?".
//!
//! Distinct from [`super::value::Folding`], which answers how several *values*
//! of a header may share a line. A reader that wants the members of a set asks
//! here and never spells a separator itself: the comma is the SIP default
//! (RFC 3261 §7.3.1), and the RFC 3323 §4.2 priv-value list is the one family
//! whose members are separated by `;`.

use crate::parser::custom::structured_headers::{top_level_entries, TopLevelEntries};

use super::name::HeaderName;

/// What separates the items inside one header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemSeparator {
    /// RFC 3261 §7.3.1 — the SIP default.
    Comma,
    /// RFC 3323 §4.2 — the priv-value list of `Privacy`.
    Semicolon,
}

impl ItemSeparator {
    /// The ASCII byte this separator is written as.
    pub const fn byte(self) -> u8 {
        match self {
            ItemSeparator::Comma => b',',
            ItemSeparator::Semicolon => b';',
        }
    }

    /// The wire text that rejoins items written with this separator.
    pub const fn joiner(self) -> &'static str {
        match self {
            ItemSeparator::Comma => ", ",
            ItemSeparator::Semicolon => ";",
        }
    }

    /// The items of `value`, trimmed and borrowed. A separator inside a
    /// quoted-string or `<...>` is data, not a separator.
    pub fn split(self, value: &str) -> TopLevelEntries<'_> {
        top_level_entries(value, self.byte())
    }
}

impl HeaderName {
    /// What separates the items of one value of this header.
    pub fn item_separator(&self) -> ItemSeparator {
        match self {
            HeaderName::Privacy => ItemSeparator::Semicolon,
            _ => ItemSeparator::Comma,
        }
    }

    /// The item separator of a wire name (any casing, compact or long form)
    /// without minting a [`HeaderName`] for an extension header — an extension
    /// header's items, when it has any, are comma-separated.
    pub fn item_separator_of(name: &str) -> ItemSeparator {
        Self::known(name).map_or(ItemSeparator::Comma, |known| known.item_separator())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_separates_its_priv_values_with_semicolons() {
        assert_eq!(HeaderName::Privacy.item_separator(), ItemSeparator::Semicolon);
        assert_eq!(HeaderName::item_separator_of("privacy"), ItemSeparator::Semicolon);
        let items: Vec<&str> =
            HeaderName::Privacy.item_separator().split("id;user; header").collect();
        assert_eq!(items, ["id", "user", "header"]);
    }

    #[test]
    fn every_other_header_separates_with_commas() {
        for name in ["Supported", "Allow", "Contact", "Via", "Reason", "X-Extension"] {
            assert_eq!(HeaderName::item_separator_of(name), ItemSeparator::Comma, "{name}");
        }
        let items: Vec<&str> =
            HeaderName::Supported.item_separator().split("100rel, timer").collect();
        assert_eq!(items, ["100rel", "timer"]);
    }

    #[test]
    fn a_separator_inside_a_quoted_string_is_data() {
        let commas: Vec<&str> =
            ItemSeparator::Comma.split("399 gw \"refused, hard\", 399 gw2 \"ok\"").collect();
        assert_eq!(commas, ["399 gw \"refused, hard\"", "399 gw2 \"ok\""]);

        let semis: Vec<&str> = ItemSeparator::Semicolon.split("id;critical=\"a;b\";user").collect();
        assert_eq!(semis, ["id", "critical=\"a;b\"", "user"]);
    }

    #[test]
    fn a_separator_inside_angle_brackets_is_data() {
        let commas: Vec<&str> = ItemSeparator::Comma.split("<sip:a@h;p=1,2>, <sip:b@h>").collect();
        assert_eq!(commas, ["<sip:a@h;p=1,2>", "<sip:b@h>"]);

        let semis: Vec<&str> = ItemSeparator::Semicolon.split("<sip:a@h;p=1>;id").collect();
        assert_eq!(semis, ["<sip:a@h;p=1>", "id"]);
    }
}
