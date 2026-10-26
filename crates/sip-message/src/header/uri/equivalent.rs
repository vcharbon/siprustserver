//! URI equivalence: RFC 3261 §19.1.4 for SIP and SIPS URIs.

use crate::char_class::is_reserved;
use crate::sip_str::SipStr;

use super::super::params::{ParamValue, Params};
use super::Uri;

/// The uri-parameters that, present in one URI only, make the two differ
/// (§19.1.4): every other one present in one URI only is ignored.
const DEFAULT_BEARING_PARAMS: [&str; 5] = ["user", "ttl", "method", "maddr", "transport"];

impl Uri {
    /// Whether `self` and `other` name the same resource under RFC 3261
    /// §19.1.4: a SIP URI never matches a SIPS one; the userinfo compares
    /// case-sensitively, every other part case-insensitively; a character
    /// outside the `reserved` set equals its `%HEX HEX` escape; the user,
    /// password, host and port match, a part present on one side only (an
    /// explicit 5060 included) making them differ; a uri-parameter present
    /// on both sides matches, and `user`, `ttl`, `method`, `maddr` or
    /// `transport` present on one side only makes them differ, any other one
    /// being ignored; the escaped headers match as sets, none ignored. No
    /// host name is resolved. Equivalence is not transitive.
    ///
    /// A URI of another scheme is equivalent to one of the same scheme, in
    /// any case, whose parts are the same as read.
    pub fn equivalent(&self, other: &Uri) -> bool {
        if !self.scheme.eq_ignore_ascii_case(&other.scheme) {
            return false;
        }
        if !self.is_sip_or_sips() {
            return self.user == other.user
                && self.authority == other.authority
                && self.params == other.params
                && self.headers == other.headers
                && self.opaque_part == other.opaque_part;
        }
        same_part(self.user.as_deref(), other.user.as_deref(), Case::Sensitive)
            && same_text(self.host(), other.host(), Case::Insensitive)
            && self.port() == other.port()
            && params_equivalent(&self.params, &other.params)
            && headers_equivalent(&self.headers, &other.headers)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Sensitive,
    Insensitive,
}

/// One unit of URI text once escapes are read: a byte, and whether it was a
/// `reserved` byte written escaped, which stays apart from the same byte
/// written plain.
type Unit = (u8, bool);

/// `text` as units: an escape of a byte outside `reserved` is that byte.
fn units(text: &str) -> Vec<Unit> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            if let [h, l, more @ ..] = tail {
                if let Some(byte) = hex_byte(*h, *l) {
                    out.push((byte, is_reserved(byte)));
                    rest = more;
                    continue;
                }
            }
        }
        out.push((b, false));
        rest = tail;
    }
    out
}

fn hex_byte(h: u8, l: u8) -> Option<u8> {
    let digit = |d: u8| char::from(d).to_digit(16).and_then(|v| u8::try_from(v).ok());
    digit(h)?.checked_mul(16)?.checked_add(digit(l)?)
}

fn same_text(a: &str, b: &str, case: Case) -> bool {
    let (a, b) = (units(a), units(b));
    a.len() == b.len()
        && a.iter().zip(&b).all(|(&(x, xe), &(y, ye))| {
            xe == ye && if case == Case::Sensitive { x == y } else { x.eq_ignore_ascii_case(&y) }
        })
}

/// Two optional parts: both absent, or both present and the same text.
fn same_part(a: Option<&str>, b: Option<&str>, case: Case) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_text(a, b, case),
        _ => false,
    }
}

fn same_value(a: &ParamValue, b: &ParamValue) -> bool {
    same_part(a.as_str(), b.as_str(), Case::Insensitive)
}

/// The value of the first parameter of `params` named `name`.
fn lookup<'a>(params: &'a Params, name: &str) -> Option<&'a ParamValue> {
    params.iter().find(|(n, _)| same_text(n, name, Case::Insensitive)).map(|(_, v)| v)
}

fn params_equivalent(a: &Params, b: &Params) -> bool {
    let one_way = |from: &Params, to: &Params| {
        from.iter().all(|(name, value)| match lookup(to, name) {
            Some(other) => same_value(value, other),
            None => !DEFAULT_BEARING_PARAMS.iter().any(|p| same_text(name, p, Case::Insensitive)),
        })
    };
    one_way(a, b) && one_way(b, a)
}

/// The escaped headers of two URIs, as sets: each of one matches a distinct
/// one of the other.
fn headers_equivalent(a: &[(SipStr, Option<SipStr>)], b: &[(SipStr, Option<SipStr>)]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut taken = vec![false; b.len()];
    a.iter().all(|(name, value)| {
        let found = b.iter().zip(taken.iter_mut()).find(|((n, v), taken)| {
            !**taken
                && same_text(name, n, Case::Insensitive)
                && same_part(value.as_deref(), v.as_deref(), Case::Insensitive)
        });
        match found {
            Some((_, taken)) => {
                *taken = true;
                true
            }
            None => false,
        }
    })
}
