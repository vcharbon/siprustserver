//! The readings of the values that name a dialog: the From / To address and
//! its tag, and the Call-ID (RFC 3261 §25.1).

use super::structured_headers::parse_name_addr;
use crate::char_class::{is_token_char, is_word_char};
use crate::error::SipParseError;
use crate::header::{AddrForm, NameAddr, ParamValue, Uri, UriRole};
use crate::sip_str::SipStr;

/// One From / To value, its URI read as a [`UriRole::Identity`]: on wire bytes
/// the strict name-addr reading, refused whole on any grammar error; in
/// `Hydrate` mode the lenient scan, its URI held [`Uri::opaque`] when the URI
/// reader refuses it.
pub(super) fn read_address(
    value: &SipStr,
    header: &str,
    wire: bool,
) -> Result<NameAddr, SipParseError> {
    if wire {
        return NameAddr::parse_as(value, AddrForm::NameAddrOrSpec, UriRole::Identity)
            .map_err(|e| SipParseError::new(format!("Strict {header}: {}", e.reason)));
    }
    let p = parse_name_addr(value);
    let uri = Uri::parse_as(&p.uri, UriRole::Identity).unwrap_or_else(|_| Uri::opaque(p.uri));
    Ok(NameAddr::from_parts(p.display_name, uri, p.params))
}

/// The dialog tag of a From / To read off the wire: `tag-param = "tag" EQUAL
/// token`.
pub(super) fn tag_refusal(addr: &NameAddr, header: &str) -> Result<(), SipParseError> {
    match addr.params().get("tag") {
        None => Ok(()),
        Some(ParamValue::Token(t)) if t.bytes().all(is_token_char) => Ok(()),
        Some(_) => Err(SipParseError::new(format!("Strict {header}: the tag is not a token"))),
    }
}

/// `callid = word [ "@" word ]`.
pub(super) fn is_call_id(value: &str) -> bool {
    let word = |w: &str| !w.is_empty() && w.bytes().all(is_word_char);
    match value.split_once('@') {
        Some((local, host)) => word(local) && word(host),
        None => word(value),
    }
}

/// Count the `;tag=` header parameters: those outside quoted-strings and
/// outside `<...>`, where a `tag` is a URI parameter (RFC 3261 permits one).
pub(super) fn count_tag_params(value: &str) -> usize {
    let s = value.as_bytes();
    let at = |k: usize| s.get(k).copied();
    let skip_ws = |mut k: usize| {
        while matches!(at(k), Some(b' ' | b'\t')) {
            k += 1;
        }
        k
    };
    let mut count = 0usize;
    let mut in_quote = false;
    let mut in_angle = false;
    let mut i = 0usize;
    while let Some(c) = at(i) {
        if in_quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if in_angle {
            in_angle = c != b'>';
            i += 1;
            continue;
        }
        if c == b'"' || c == b'<' {
            in_quote = c == b'"';
            in_angle = c == b'<';
            i += 1;
            continue;
        }
        if c != b';' {
            i += 1;
            continue;
        }
        let j = skip_ws(i + 1);
        let is_tag = s.get(j..j + 3).is_some_and(|name| name.eq_ignore_ascii_case(b"tag"));
        if !is_tag || at(skip_ws(j + 3)) != Some(b'=') {
            i += 1;
            continue;
        }
        count += 1;
        i += 1;
    }
    count
}
