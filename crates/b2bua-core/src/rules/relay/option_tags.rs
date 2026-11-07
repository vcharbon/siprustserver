//! The call's option-tag offer and withhold on the assembled headers of a leg
//! it originates ([`super::build_b_leg`]).

use sip_message::header::{self, HeaderName, HeaderValue, TokenListHeader};
use sip_message::{SipHeader as MsgHeader, SipParseError, SipRequest, SipStr};

/// Whether `req` offers `100rel` (RFC 3262 §3). Ingress refuses a `Require`
/// or `Supported` that does not read, so one here is a defect: logged, and
/// read as offering nothing.
pub(crate) fn offers_100rel(req: &SipRequest) -> bool {
    req.offers_100rel().unwrap_or_else(|e| {
        unreadable_option_tags(&HeaderName::Supported, &e);
        false
    })
}

/// Log an option-tag line of `header` that does not read past ingress, which
/// refuses one: a defect of whatever built the line, which stays as written.
pub(super) fn unreadable_option_tags(header: &HeaderName, error: &SipParseError) {
    tracing::error!(
        header = header.as_wire_str(),
        reason = %error.reason,
        "DEFECT: an option-tag line past ingress does not read; it is left as written"
    );
}

/// The set the lines of `K` among `headers` state together (`None`: no
/// line), or `Err` when one does not read, which [`unreadable_option_tags`]
/// has logged.
fn stated_set<K: header::kind::TokenKind>(
    headers: &[MsgHeader],
) -> Result<Option<TokenListHeader<K>>, ()> {
    let name = K::name();
    let lines = headers
        .iter()
        .filter(|h| name.matches(&h.name))
        .map(|h| TokenListHeader::<K>::parse(&h.value))
        .collect::<Result<Vec<_>, _>>();
    match lines {
        Ok(lines) => Ok(TokenListHeader::combine(lines)),
        Err(e) => {
            unreadable_option_tags(&name, &e);
            Err(())
        }
    }
}

/// The call's own offer on an originated leg's assembled headers: the
/// `Supported` set states every tag in `offered`, once — an absent line comes
/// into being for them, a line already naming them all is left byte-identical.
/// A line that does not read leaves the headers as they are (a defect,
/// [`unreadable_option_tags`]).
pub(super) fn apply_offered_option_tags(extra_headers: &mut Vec<MsgHeader>, offered: &[String]) {
    if offered.is_empty() {
        return;
    }
    let name = <header::kind::Supported as header::kind::HeaderKind>::name();
    let Ok(stated) = stated_set::<header::kind::Supported>(extra_headers) else {
        return;
    };
    let stated = stated.unwrap_or_else(TokenListHeader::empty);
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
/// absent line already says (§20.37). A line that does not read leaves the
/// headers as they are (a defect, [`unreadable_option_tags`]).
fn narrow_token_set<K: header::kind::TokenKind>(
    extra_headers: &mut Vec<MsgHeader>,
    withheld: &[String],
) {
    let name = K::name();
    let Ok(Some(set)) = stated_set::<K>(extra_headers) else {
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
