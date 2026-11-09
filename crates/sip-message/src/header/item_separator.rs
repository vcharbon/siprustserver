//! The separator a header's own grammar puts between the ITEMS of one value —
//! the crate's single answer to "what splits this header's list?".
//!
//! Distinct from [`super::value::Folding`], which answers how several *values*
//! of a header may share a line. A reader that wants the members of a set asks
//! here and never spells a separator itself: the comma is the SIP default
//! (RFC 3261 §7.3.1), and the RFC 3323 priv-value list is the one family
//! whose members are separated by `;` (§4.2) or `,` (erratum 5184).

use crate::parser::custom::structured_headers::top_level_entries;

use super::name::HeaderName;

/// What separates the items inside one header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemSeparator {
    /// RFC 3261 §7.3.1 — the SIP default.
    Comma,
    /// The priv-value list of `Privacy`: `;` as RFC 3323 §4.2 publishes it,
    /// or `,` as its erratum 5184 (Verified) writes it.
    PrivValue,
}

impl ItemSeparator {
    /// The ASCII bytes that separate two items.
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            ItemSeparator::Comma => b",",
            ItemSeparator::PrivValue => b",;",
        }
    }

    /// The wire text that joins items this stack writes: a comma, which every
    /// list this crate writes separates with (RFC 3323 erratum 5184 for the
    /// priv-values).
    pub const fn joiner(self) -> &'static str {
        ", "
    }

    /// The items of `value`, trimmed and borrowed. A separator inside a
    /// quoted-string or `<...>` is data, not a separator.
    pub fn split(self, value: &str) -> impl Iterator<Item = &str> {
        top_level_entries(value, self)
    }
}

impl HeaderName {
    /// What separates the items of one value of this header.
    pub fn item_separator(&self) -> ItemSeparator {
        match self {
            HeaderName::Privacy => ItemSeparator::PrivValue,
            _ => ItemSeparator::Comma,
        }
    }

    /// The item separator of a wire name (any casing, compact or long form)
    /// without minting a [`HeaderName`] for an extension header — an extension
    /// header's items, when it has any, are comma-separated.
    pub fn item_separator_of(name: &str) -> ItemSeparator {
        Self::known(name).map_or(ItemSeparator::Comma, |known| known.item_separator())
    }

    /// Whether one row of this header is one item, a comma in it data: the
    /// headers RFC 3261 §7.3.1 exempts from comma-folding (the credentials
    /// and challenge family, and Authentication-Info whose parameters are
    /// comma-separated inside one value), and those whose grammar is a single
    /// value, never a comma list (Date's `rfc1123-date` holds a comma).
    pub fn one_item_per_row(&self) -> bool {
        matches!(
            self,
            HeaderName::Authorization
                | HeaderName::ProxyAuthorization
                | HeaderName::WwwAuthenticate
                | HeaderName::ProxyAuthenticate
                | HeaderName::AuthenticationInfo
                | HeaderName::CallId
                | HeaderName::ContentDisposition
                | HeaderName::ContentLength
                | HeaderName::ContentTransferEncoding
                | HeaderName::ContentType
                | HeaderName::CSeq
                | HeaderName::Date
                | HeaderName::Event
                | HeaderName::Expires
                | HeaderName::From
                | HeaderName::GeolocationError
                | HeaderName::GeolocationRouting
                | HeaderName::MaxForwards
                | HeaderName::MimeVersion
                | HeaderName::MinExpires
                | HeaderName::MinSe
                | HeaderName::Organization
                | HeaderName::Priority
                | HeaderName::RAck
                | HeaderName::ReferSub
                | HeaderName::ReferTo
                | HeaderName::ReferredBy
                | HeaderName::Replaces
                | HeaderName::ReplyTo
                | HeaderName::RetryAfter
                | HeaderName::RSeq
                | HeaderName::Server
                | HeaderName::SessionExpires
                | HeaderName::Subject
                | HeaderName::SubscriptionState
                | HeaderName::Timestamp
                | HeaderName::To
                | HeaderName::UserAgent
        )
    }

    /// [`one_item_per_row`](Self::one_item_per_row) of a wire name (any
    /// casing, compact or long form); an extension header's rows are comma
    /// lists.
    pub fn one_item_per_row_of(name: &str) -> bool {
        Self::known(name).is_some_and(|known| known.one_item_per_row())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_separates_its_priv_values_with_semicolons_or_commas() {
        assert_eq!(HeaderName::Privacy.item_separator(), ItemSeparator::PrivValue);
        assert_eq!(HeaderName::item_separator_of("privacy"), ItemSeparator::PrivValue);
        let items: Vec<&str> =
            HeaderName::Privacy.item_separator().split("id;user, header").collect();
        assert_eq!(items, ["id", "user", "header"]);
    }

    #[test]
    fn the_folding_exempt_and_single_valued_headers_hold_one_item_per_row() {
        for name in ["WWW-Authenticate", "authorization", "Proxy-Authenticate", "Date", "f", "i"] {
            assert!(HeaderName::one_item_per_row_of(name), "{name}");
        }
        for name in ["Diversion", "Via", "Reason", "Supported", "X-Extension"] {
            assert!(!HeaderName::one_item_per_row_of(name), "{name}");
        }
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

        let semis: Vec<&str> = ItemSeparator::PrivValue.split("id;critical=\"a;b\";user").collect();
        assert_eq!(semis, ["id", "critical=\"a;b\"", "user"]);
    }

    #[test]
    fn a_separator_inside_angle_brackets_is_data() {
        let commas: Vec<&str> = ItemSeparator::Comma.split("<sip:a@h;p=1,2>, <sip:b@h>").collect();
        assert_eq!(commas, ["<sip:a@h;p=1,2>", "<sip:b@h>"]);

        let semis: Vec<&str> = ItemSeparator::PrivValue.split("<sip:a@h;p=1>;id").collect();
        assert_eq!(semis, ["<sip:a@h;p=1>", "id"]);
    }
}
