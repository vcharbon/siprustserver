//! The call's option-tag offer and withhold on the assembled headers of a leg
//! it originates ([`super::build_b_leg`]).

use sip_message::header::{self, HeaderValue, TokenListHeader};
use sip_message::{SipHeader as MsgHeader, SipStr};

/// The call's own offer on an originated leg's assembled headers: the
/// `Supported` set states every tag in `offered`, once — an absent line comes
/// into being for them, a line already naming them all is left byte-identical.
pub(super) fn apply_offered_option_tags(extra_headers: &mut Vec<MsgHeader>, offered: &[String]) {
    if offered.is_empty() {
        return;
    }
    let name = <header::kind::Supported as header::kind::HeaderKind>::name();
    let lines: Vec<TokenListHeader<header::kind::Supported>> = extra_headers
        .iter()
        .filter(|h| name.matches(&h.name))
        .filter_map(|h| TokenListHeader::parse(&h.value).ok())
        .collect();
    let stated = TokenListHeader::combine(lines).unwrap_or_else(TokenListHeader::empty);
    if offered.iter().all(|tag| stated.contains(tag)) {
        return;
    }
    let widened = offered.iter().fold(stated, |s, tag| s.with(tag.as_str()));
    extra_headers.retain(|h| !name.matches(&h.name));
    extra_headers.push(MsgHeader {
        name: SipStr::owned(name.as_wire_str()),
        value: SipStr::owned(&widened.to_wire()),
    });
}

/// The call-scoped withhold on an originated leg's assembled headers: the
/// `Supported` and `Require` sets are narrowed by `withheld`, and a set the
/// narrowing empties drops its header — a withheld tag leaves ONE wire form on
/// every leg the call originates, whichever mint assembled it. Lines not naming
/// a withheld tag are left byte-identical.
pub(crate) fn apply_withheld_option_tags(extra_headers: &mut Vec<MsgHeader>, withheld: &[String]) {
    if withheld.is_empty() {
        return;
    }
    narrow_token_set::<header::kind::Supported>(extra_headers, withheld);
    narrow_token_set::<header::kind::Require>(extra_headers, withheld);
}

/// Narrow one token-set header's lines by `withheld`, in place: no line, or
/// none naming a withheld tag, leaves the headers untouched; otherwise the
/// lines collapse to ONE restated line without the withheld tags (several
/// lines of a set-like header are one set, RFC 3261 §7.3.1), and a set left
/// empty drops its header — an emptied set claims nothing, which is what an
/// absent line already says (§20.37).
fn narrow_token_set<K: header::kind::TokenKind>(
    extra_headers: &mut Vec<MsgHeader>,
    withheld: &[String],
) {
    let name = K::name();
    let lines: Vec<TokenListHeader<K>> = extra_headers
        .iter()
        .filter(|h| name.matches(&h.name))
        .filter_map(|h| TokenListHeader::<K>::parse(&h.value).ok())
        .collect();
    let Some(set) = TokenListHeader::combine(lines) else {
        return;
    };
    if !withheld.iter().any(|tag| set.contains(tag)) {
        return;
    }
    let kept = withheld.iter().fold(set, |s, tag| s.without(tag));
    extra_headers.retain(|h| !name.matches(&h.name));
    if !kept.is_empty() {
        extra_headers.push(MsgHeader {
            name: SipStr::owned(name.as_wire_str()),
            value: SipStr::owned(&kept.to_wire()),
        });
    }
}
