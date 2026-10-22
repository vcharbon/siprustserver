//! The one URI reader: text in, the parts of a [`Uri`](super::Uri) out, or a
//! refusal. Validation and the typed value come from the same cut, so the URI
//! a gate accepts is the URI a router reads.
//!
//! A `sip` / `sips` URI is read by the RFC 3261 §25.1 grammar whole: every byte
//! after the scheme lands in the userinfo, host, port, a parameter or an
//! escaped header, each held to its character class, or the URI is refused.
//! A `tel` URI keeps a structural reading, its number as the host. Any other
//! scheme is opaque: its scheme-specific part is kept whole and names no host.
//! Both are held only to the URI character set ([`is_uri_char`], and a `tel`
//! number's `#`). The URI's [`UriRole`] moves one rule: a digit-led top label.
//!
//! The same host reader reads a Via sent-by ([`read_sent_by`]).

use std::net::Ipv6Addr;

use crate::char_class::{
    is_alphanum, is_hnv_char, is_paramchar, is_password_char, is_uri_char, is_user_char,
    run_refusal,
};
use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::super::params::{parse_semicolon_params, ParamValue, Params, URI_PARAMS};
use super::super::scan::{byte_at, clamped, index_of, scan_until, skip_ws, sub, sub_lower};
use super::{HostPort, UriRole};

/// The parts of one URI, cut from the text it was read from.
pub(super) struct UriParts {
    pub(super) scheme: SipStr,
    pub(super) user: Option<SipStr>,
    pub(super) authority: HostPort,
    pub(super) params: Params,
    pub(super) headers: Vec<(SipStr, Option<SipStr>)>,
    /// The scheme-specific part of a URI of an opaque scheme, whole.
    pub(super) opaque: Option<SipStr>,
}

/// Read all of `value` as one URI.
pub(super) fn read(value: &SipStr, role: UriRole) -> Result<UriParts, SipParseError> {
    let bytes = value.as_bytes();
    let colon = index_of(bytes, b':', 0)
        .ok_or_else(|| SipParseError::new(format!("URI has no scheme: {:?}", value.as_str())))?;
    scheme_refusal(clamped(value.as_str(), 0, colon)).map_or(Ok(()), |r| Err(fail(value, &r)))?;
    let scheme = sub_lower(value, 0, colon);
    let after = colon + 1;
    if scheme.eq_ignore_ascii_case("sip") || scheme.eq_ignore_ascii_case("sips") {
        read_sip(value, scheme, after, role).map_err(|reason| fail(value, &reason))
    } else if scheme.eq_ignore_ascii_case("tel") {
        read_tel(value, scheme, after)
    } else {
        read_opaque(value, scheme, after)
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
fn read_sip(value: &SipStr, scheme: SipStr, i: usize, role: UriRole) -> Result<UriParts, String> {
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
    let (authority, mut j) = read_hostport(value, host_at, role)?;

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
    Ok(UriParts { scheme, user, authority, params, headers, opaque: None })
}

/// `userinfo =( user / telephone-subscriber ) [ ":" password ] "@"`, the `@`
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

/// `host` from byte `i`, where a byte of `ends` or a `:` ends a host that is
/// no IPv6 reference; yields the host, an IPv6 address without its brackets,
/// and the position after it.
fn read_host(
    value: &SipStr,
    i: usize,
    role: UriRole,
    ends: &[u8],
) -> Result<(SipStr, usize), String> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    if byte_at(bytes, i) == Some(b'[') {
        let close =
            index_of(bytes, b']', i + 1).ok_or_else(|| "unclosed IPv6 reference".to_string())?;
        let inner = clamped(text, i + 1, close);
        if inner.is_empty() {
            return Err("empty IPv6 reference".to_string());
        }
        if !is_ipv6_address(inner) {
            return Err(format!("invalid IPv6 reference [{inner}]"));
        }
        return Ok((sub(value, i + 1, close), close + 1));
    }
    let end = scan_until_host_end(bytes, i, ends);
    let hostport_end = scan_until(bytes, end, ends);
    if index_of(bytes, b':', end + 1).is_some_and(|c| c < hostport_end) {
        return Err("unbracketed IPv6 host (RFC 3261 §19.1.1 requires [])".to_string());
    }
    host_refusal(clamped(text, i, end), role).map_or(Ok(()), Err)?;
    Ok((sub(value, i, end), end))
}

/// The first byte at or after `i` that is a `:` or one of `ends`, or the end.
fn scan_until_host_end(bytes: &[u8], i: usize, ends: &[u8]) -> usize {
    bytes
        .get(i..)
        .and_then(|rest| rest.iter().position(|b| *b == b':' || ends.contains(b)))
        .map_or(bytes.len().max(i), |p| i + p)
}

/// `hostport = host [ ":" port ]` from byte `i`; yields the value and the
/// position after it, which is a `;`, a `?` or the end.
fn read_hostport(value: &SipStr, i: usize, role: UriRole) -> Result<(HostPort, usize), String> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    let (host, j) = read_host(value, i, role, b";?")?;
    match byte_at(bytes, j) {
        None | Some(b';' | b'?') => Ok((HostPort::new(host, None), j)),
        Some(b':') => {
            let end = scan_until(bytes, j + 1, b";?");
            Ok((HostPort::new(host, Some(port_value(clamped(text, j + 1, end))?)), end))
        }
        Some(b) => Err(format!("byte 0x{b:02x} after the host")),
    }
}

/// `sent-by = host [ COLON port ]` (RFC 3261 §20.42, `COLON = SWS ":" SWS`)
/// from byte `i` of one Via value, the host read as a [`UriRole::Target`]
/// URI host. Yields the host, the port's digits as a number (its range is the
/// Via's to judge) and the position after the sent-by, where only LWS, a `;`
/// or a `,` may follow.
pub(crate) fn read_sent_by(
    value: &SipStr,
    i: usize,
) -> Result<(SipStr, Option<u64>, usize), String> {
    const ENDS: &[u8] = b";, \t";
    let bytes = value.as_bytes();
    let (host, mut j) = read_host(value, i, UriRole::Target, ENDS)?;
    let mut port = None;
    let colon = skip_ws(bytes, j);
    if byte_at(bytes, colon) == Some(b':') {
        let at = skip_ws(bytes, colon + 1);
        let end = scan_until(bytes, at, ENDS);
        let digits = clamped(value.as_str(), at, end);
        if digits.is_empty() {
            return Err("empty port".to_string());
        }
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("non-digit in port {digits:?}"));
        }
        let number = digits.bytes().fold(0u64, |n, d| {
            n.saturating_mul(10).saturating_add(u64::from(d.saturating_sub(b'0')))
        });
        port = Some(number);
        j = end;
    }
    match byte_at(bytes, skip_ws(bytes, j)) {
        None | Some(b';' | b',') => Ok((host, port, j)),
        Some(b) => Err(format!("byte 0x{b:02x} after the sent-by")),
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
/// `host = hostname / IPv4address / IPv6reference` in `role`, if it is not.
pub(crate) fn host_refusal(host: &str, role: UriRole) -> Option<String> {
    if host.is_empty() {
        return Some("empty host".to_string());
    }
    if host.contains(':') {
        return (!is_ipv6_address(host)).then(|| format!("invalid IPv6 address {host:?}"));
    }
    if is_dotted_digits(host.strip_suffix('.').unwrap_or(host)) {
        return ipv4_refusal(host);
    }
    hostname_refusal(host, role)
}

/// Two or more non-empty runs of digits joined by dots: an `IPv4address`,
/// or no host at all.
fn is_dotted_digits(host: &str) -> bool {
    host.contains('.')
        && host.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
}

/// `hostname = *( domainlabel "." ) toplabel [ "." ]`, where a label is
/// alphanumerics and inner hyphens and a [`UriRole::Target`]'s top label
/// starts with a letter. An [`UriRole::Identity`]'s may start with a digit
/// (RFC 1123 §2.1), the one leniency of the host grammar (ADR-0053).
fn hostname_refusal(host: &str, role: UriRole) -> Option<String> {
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
    let letter_led = top.is_some_and(|b| b.is_ascii_alphabetic());
    (role == UriRole::Target && !letter_led)
        .then(|| format!("top label of {host:?} must start with ALPHA"))
}

/// A dotted run of digits as an `IPv4address`: four octets of at most three
/// digits, at most 255, and no leading zero (an octal reading of one names
/// another host).
fn ipv4_refusal(host: &str) -> Option<String> {
    if host.split('.').count() != 4 || host.split('.').any(str::is_empty) {
        return Some(format!("malformed IPv4 address {host:?}"));
    }
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

/// `IPv6address` in the RFC 4291 §2.2 text form, the one RFC 5954 §4.1 puts
/// in place of the RFC 3261 §25.1 rule. No zone ID.
fn is_ipv6_address(text: &str) -> bool {
    text.parse::<Ipv6Addr>().is_ok()
}

/// A `tel` URI (RFC 3966), read structurally: every byte of the URI character
/// set or a raw `#` (`local-number-digits`), the number up to the first `;`
/// or `?` as the host, then `;`-parameters and `?`-pairs.
fn read_tel(value: &SipStr, scheme: SipStr, i: usize) -> Result<UriParts, SipParseError> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    let class: fn(u8) -> bool = |b| is_uri_char(b) || b == b'#';
    if let Some(r) = run_refusal(clamped(text, i, bytes.len()), class, false) {
        return Err(fail(value, &r));
    }
    let number_end = scan_until(bytes, i, b";?");
    let authority = HostPort::new(sub(value, i, number_end), None);
    let (params, after_params) = parse_semicolon_params(value, number_end, &URI_PARAMS);
    let mut headers = Vec::new();
    if byte_at(bytes, after_params) == Some(b'?') {
        for pair in clamped(text, after_params + 1, bytes.len()).split('&') {
            match pair.split_once('=') {
                Some((name, val)) => headers.push((value.reslice(name), Some(value.reslice(val)))),
                None => headers.push((value.reslice(pair), None)),
            }
        }
    }
    Ok(UriParts { scheme, user: None, authority, params, headers, opaque: None })
}

/// A URI of a scheme this stack does not take apart (`urn`, `http`, `cid` …):
/// every byte of the URI character set, the scheme-specific part kept whole.
/// It names no user, host, port, parameter or escaped header.
fn read_opaque(value: &SipStr, scheme: SipStr, i: usize) -> Result<UriParts, SipParseError> {
    let len = value.as_str().len();
    if let Some(r) = run_refusal(clamped(value.as_str(), i, len), is_uri_char, false) {
        return Err(fail(value, &r));
    }
    Ok(UriParts {
        scheme,
        user: None,
        authority: HostPort::new(SipStr::EMPTY, None),
        params: Params::new(),
        headers: Vec::new(),
        opaque: Some(sub(value, i, len)),
    })
}
