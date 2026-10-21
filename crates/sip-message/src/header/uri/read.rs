//! The one URI reader: text in, the parts of a [`Uri`](super::Uri) out, or a
//! refusal. Validation and the typed value come from the same cut, so the URI
//! a gate accepts is the URI a router reads.
//!
//! A `sip` / `sips` URI is read by the RFC 3261 §25.1 grammar whole: every byte
//! after the scheme lands in the userinfo, host, port, a parameter or an
//! escaped header, each held to its character class, or the URI is refused.
//! Another scheme (`tel`, `urn`, …) keeps a generic structural reading and is
//! held only to the URI character set ([`is_uri_char`]).

use std::net::Ipv6Addr;

use crate::char_class::{
    is_alphanum, is_hnv_char, is_paramchar, is_password_char, is_uri_char, is_user_char,
    run_refusal,
};
use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::super::params::{parse_semicolon_params, ParamValue, Params, URI_PARAMS};
use super::super::scan::{byte_at, clamped, index_of, scan_until, sub, sub_lower};
use super::HostPort;

/// The parts of one URI, cut from the text it was read from.
pub(super) struct UriParts {
    pub(super) scheme: SipStr,
    pub(super) user: Option<SipStr>,
    pub(super) authority: HostPort,
    pub(super) params: Params,
    pub(super) headers: Vec<(SipStr, Option<SipStr>)>,
}

/// Read all of `value` as one URI.
pub(super) fn read(value: &SipStr) -> Result<UriParts, SipParseError> {
    let bytes = value.as_bytes();
    let colon = index_of(bytes, b':', 0)
        .ok_or_else(|| SipParseError::new(format!("URI has no scheme: {:?}", value.as_str())))?;
    scheme_refusal(clamped(value.as_str(), 0, colon)).map_or(Ok(()), |r| Err(fail(value, &r)))?;
    let scheme = sub_lower(value, 0, colon);
    let after = colon + 1;
    if scheme.eq_ignore_ascii_case("sip") || scheme.eq_ignore_ascii_case("sips") {
        read_sip(value, scheme, after).map_err(|reason| fail(value, &reason))
    } else {
        read_other(value, scheme, after)
    }
}

fn fail(value: &SipStr, reason: &str) -> SipParseError {
    SipParseError::new(format!("{reason} in URI {:?}", value.as_str()))
}

/// `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn scheme_refusal(scheme: &str) -> Option<String> {
    let mut bytes = scheme.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() => {}
        _ => return Some("scheme must start with ALPHA".to_string()),
    }
    bytes
        .any(|b| !(is_alphanum(b) || matches!(b, b'+' | b'-' | b'.')))
        .then(|| "invalid scheme character".to_string())
}

/// `SIP-URI = "sip:" [ userinfo ] hostport uri-parameters [ headers ]`, read
/// from byte `i` to the end of `value`.
fn read_sip(value: &SipStr, scheme: SipStr, i: usize) -> Result<UriParts, String> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    // No byte after the userinfo may be an `@` (none of the classes of host,
    // port, parameters or headers holds one), so the first `@` ends it.
    let (user, host_at) = match index_of(bytes, b'@', i) {
        Some(at) => {
            userinfo_refusal(clamped(text, i, at)).map_or(Ok(()), Err)?;
            (Some(sub(value, i, at)), at + 1)
        }
        None => (None, i),
    };
    let (authority, mut j) = read_hostport(value, host_at)?;

    let mut params = Params::new();
    while byte_at(bytes, j) == Some(b';') {
        let name_at = j + 1;
        let name_end = scan_until(bytes, name_at, b"=;?");
        paramchar_refusal("uri-parameter name", clamped(text, name_at, name_end))?;
        let name = sub(value, name_at, name_end);
        if byte_at(bytes, name_end) == Some(b'=') {
            let value_end = scan_until(bytes, name_end + 1, b";?");
            paramchar_refusal("uri-parameter value", clamped(text, name_end + 1, value_end))?;
            params.push(name, ParamValue::Token(sub(value, name_end + 1, value_end)));
            j = value_end;
        } else {
            params.push(name, ParamValue::Flag);
            j = name_end;
        }
    }

    let mut headers = Vec::new();
    if byte_at(bytes, j) == Some(b'?') {
        for pair in clamped(text, j + 1, bytes.len()).split('&') {
            let (name, hvalue) = pair
                .split_once('=')
                .ok_or_else(|| format!("escaped header {pair:?} has no `=`"))?;
            if name.is_empty() {
                return Err(format!("escaped header {pair:?} has no name"));
            }
            if let Some(r) = run_refusal(name, is_hnv_char, true) {
                return Err(format!("escaped header name: {r}"));
            }
            if let Some(r) = run_refusal(hvalue, is_hnv_char, true) {
                return Err(format!("escaped header value: {r}"));
            }
            headers.push((value.reslice(name), Some(value.reslice(hvalue))));
        }
    }
    Ok(UriParts { scheme, user, authority, params, headers })
}

/// `userinfo = ( user / telephone-subscriber ) [ ":" password ] "@"`, the `@`
/// excluded. A telephone-subscriber's characters are all user characters.
fn userinfo_refusal(userinfo: &str) -> Option<String> {
    let (user, password) = match userinfo.split_once(':') {
        Some((user, password)) => (user, Some(password)),
        None => (userinfo, None),
    };
    if user.is_empty() {
        return Some("empty user before `@`".to_string());
    }
    if let Some(r) = run_refusal(user, is_user_char, true) {
        return Some(format!("user: {r}"));
    }
    password.and_then(|p| run_refusal(p, is_password_char, true)).map(|r| format!("password: {r}"))
}

/// `1*paramchar`.
fn paramchar_refusal(what: &str, run: &str) -> Result<(), String> {
    if run.is_empty() {
        return Err(format!("empty {what}"));
    }
    run_refusal(run, is_paramchar, true).map_or(Ok(()), |r| Err(format!("{what}: {r}")))
}

/// `hostport = host [ ":" port ]` from byte `i`; yields the value and the
/// position after it, which is a `;`, a `?` or the end.
fn read_hostport(value: &SipStr, i: usize) -> Result<(HostPort, usize), String> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    let (host, j) = if byte_at(bytes, i) == Some(b'[') {
        let close =
            index_of(bytes, b']', i + 1).ok_or_else(|| "unclosed IPv6 reference".to_string())?;
        let inner = clamped(text, i + 1, close);
        if inner.is_empty() {
            return Err("empty IPv6 reference".to_string());
        }
        if !is_ipv6_address(inner) {
            return Err(format!("invalid IPv6 reference [{inner}]"));
        }
        (sub(value, i + 1, close), close + 1)
    } else {
        let end = scan_until(bytes, i, b":;?");
        let hostport_end = scan_until(bytes, end, b";?");
        if index_of(bytes, b':', end + 1).is_some_and(|c| c < hostport_end) {
            return Err("unbracketed IPv6 host (RFC 3261 §19.1.1 requires [])".to_string());
        }
        let host = clamped(text, i, end);
        host_refusal(host).map_or(Ok(()), Err)?;
        (sub(value, i, end), end)
    };
    match byte_at(bytes, j) {
        None | Some(b';' | b'?') => Ok((HostPort::new(host, None), j)),
        Some(b':') => {
            let end = scan_until(bytes, j + 1, b";?");
            Ok((HostPort::new(host, Some(port_value(clamped(text, j + 1, end))?)), end))
        }
        Some(b) => Err(format!("byte 0x{b:02x} after the host")),
    }
}

/// `port = 1*DIGIT`, 1 to 65535: port 0 names no socket (ADR-0001).
fn port_value(port: &str) -> Result<u16, String> {
    if port.is_empty() {
        return Err("empty port".to_string());
    }
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("non-digit in port {port:?}"));
    }
    match port.parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!("port out of range: {port}")),
    }
}

/// Why `host` (an IPv6 reference without its brackets) is no
/// `host = hostname / IPv4address / IPv6reference`, if it is not.
pub(crate) fn host_refusal(host: &str) -> Option<String> {
    if host.is_empty() {
        return Some("empty host".to_string());
    }
    if host.contains(':') {
        return (!is_ipv6_address(host)).then(|| format!("invalid IPv6 address {host:?}"));
    }
    if is_ipv4_shaped(host) {
        return ipv4_refusal(host);
    }
    hostname_refusal(host)
}

/// Four non-empty runs of digits joined by dots: an `IPv4address`, or no host.
fn is_ipv4_shaped(host: &str) -> bool {
    host.split('.').count() == 4
        && host.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
}

/// `hostname = *( domainlabel "." ) toplabel [ "." ]`, where a label is
/// alphanumerics and inner hyphens and the top label starts with a letter.
fn hostname_refusal(host: &str) -> Option<String> {
    let named = host.strip_suffix('.').filter(|h| !h.is_empty()).unwrap_or(host);
    let mut top = None;
    for label in named.split('.') {
        let bytes = label.as_bytes();
        let ends_ok = bytes.first().is_some_and(|&b| is_alphanum(b))
            && bytes.last().is_some_and(|&b| is_alphanum(b));
        if !ends_ok || !bytes.iter().all(|&b| is_alphanum(b) || b == b'-') {
            return Some(format!("invalid host label {label:?}"));
        }
        top = bytes.first().copied();
    }
    (!top.is_some_and(|b| b.is_ascii_alphabetic()))
        .then(|| format!("top label of {host:?} must start with ALPHA"))
}

/// The octets of an IPv4-shaped host: at most three digits, at most 255, and
/// no leading zero (an octal reading of one names another host).
fn ipv4_refusal(host: &str) -> Option<String> {
    for octet in host.split('.') {
        if octet.len() > 1 && octet.starts_with('0') {
            return Some(format!(
                "IPv4 octet \"{octet}\" has leading zero (octal-confusion vector)"
            ));
        }
        if octet.len() > 3 {
            return Some(format!("IPv4 octet \"{octet}\" exceeds 1*3DIGIT"));
        }
        if octet.parse::<u8>().is_err() {
            return Some(format!("IPv4 octet {octet} out of range"));
        }
    }
    None
}

/// `IPv6address` as RFC 3261 §25.1 writes it, or in the RFC 4291 text form
/// RFC 5954 replaces it with (RFC 5118 §4.10 asks for both). No zone ID.
fn is_ipv6_address(text: &str) -> bool {
    is_rfc3261_ipv6(text) || text.parse::<Ipv6Addr>().is_ok()
}

/// `IPv6address = hexpart [ ":" IPv4address ]`, `hexpart = hexseq / hexseq
/// "::" [ hexseq ] / "::" [ hexseq ]`, `hexseq = hex4 *( ":" hex4 )`.
fn is_rfc3261_ipv6(text: &str) -> bool {
    let hexpart = if text.contains('.') {
        let Some((hex, v4)) = text.rsplit_once(':') else { return false };
        if !is_ipv4_shaped(v4) || ipv4_refusal(v4).is_some() {
            return false;
        }
        hex
    } else {
        text
    };
    let hexseq = |s: &str| {
        s.split(':').all(|h| (1..=4).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    match hexpart.split_once("::") {
        Some((a, b)) => (a.is_empty() || hexseq(a)) && (b.is_empty() || hexseq(b)),
        None => hexseq(hexpart),
    }
}

/// A URI of a scheme this stack does not read by its own grammar: every byte
/// of the URI character set, then a structural `user@host:port;params?headers`
/// reading that leaves what it does not recognise in the source text.
fn read_other(value: &SipStr, scheme: SipStr, i: usize) -> Result<UriParts, SipParseError> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    if let Some(r) = run_refusal(clamped(text, i, bytes.len()), is_uri_char, false) {
        return Err(fail(value, &r));
    }
    let mut i = i;
    let at = scan_until(bytes, i, b"@");
    let user = if byte_at(bytes, at) == Some(b'@') {
        let u = sub(value, i, at);
        i = at + 1;
        Some(u)
    } else {
        None
    };
    let (authority, after_host) = HostPort::parse_at(value, i)?;
    let (params, after_params) = parse_semicolon_params(value, after_host, &URI_PARAMS);
    let mut headers = Vec::new();
    if byte_at(bytes, after_params) == Some(b'?') {
        for pair in clamped(text, after_params + 1, bytes.len()).split('&') {
            match pair.split_once('=') {
                Some((name, val)) => headers.push((value.reslice(name), Some(value.reslice(val)))),
                None => headers.push((value.reslice(pair), None)),
            }
        }
    }
    Ok(UriParts { scheme, user, authority, params, headers })
}
