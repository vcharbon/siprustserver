//! [`Params`] — an ordered parameter list, and [`ParamValue`] — what one
//! parameter carries.
//!
//! Wire order round-trips faithfully because the list *is* the wire order, and
//! the common one-or-two-parameter case needs no heap node. Names keep their
//! wire spelling; every lookup is case-insensitive, which is what RFC 3261
//! §7.3.1 actually specifies.

use smallvec::SmallVec;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::scan::{
    byte_at, clamped, index_of, quoted_close, read_quoted, scan_until, skip_ws, sub, token_end,
};
use super::wire::Wire;
use crate::ip_literal::ipv6_address;

/// What a parameter carries: nothing (`;lr`), a bare token (`;branch=z9hG4bK1`)
/// or a quoted string (`;reason="call completed"`).
///
/// The token/quoted split is part of the value, not a rendering accident: it is
/// what lets `parse(render(v)) == v` hold for a value whose text contains a
/// separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    Flag,
    Token(SipStr),
    /// The UNESCAPED text; [`render`](Params::render) re-quotes and re-escapes.
    Quoted(SipStr),
}

impl ParamValue {
    /// The text, or `None` for a bare flag.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ParamValue::Flag => None,
            ParamValue::Token(v) | ParamValue::Quoted(v) => Some(v.as_str()),
        }
    }

    pub fn is_flag(&self) -> bool {
        matches!(self, ParamValue::Flag)
    }

    /// A token value, quoting it only if the text could not survive bare.
    pub fn text(value: impl Into<SipStr>) -> Self {
        let v: SipStr = value.into();
        if v.is_empty()
            || v.bytes()
                .any(|b| matches!(b, b';' | b',' | b'"' | b'>' | b' ' | b'\t' | b'=' | b'?'))
        {
            ParamValue::Quoted(v)
        } else {
            ParamValue::Token(v)
        }
    }

    fn render(&self, out: &mut Wire) {
        match self {
            ParamValue::Flag => {}
            ParamValue::Token(v) => {
                out.byte(b'=');
                out.str(v.as_str());
            }
            ParamValue::Quoted(v) => {
                out.byte(b'=');
                out.quoted(v.as_str());
            }
        }
    }
}

/// An ordered `name[=value]` list. Lookups are linear and case-insensitive —
/// parameter lists are short, and a map would cost a node block plus the wire
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(SmallVec<[(SipStr, ParamValue); 2]>);

impl Params {
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Read a header VALUE that is itself a parameter list — the first item
    /// carries no leading `;` (the P-Charging-Vector family,
    /// `icid-value=…;icid-generated-at=…`), the rest are `;`-separated. A
    /// leading segment that is not a `name=value` pair reads as a flag named
    /// after it, so a value with a leading token (`Q.850;cause=16`) still
    /// answers for its parameters.
    pub fn parse_list(raw: &SipStr) -> Self {
        let value = raw.trimmed();
        let mut params = Self::new();
        let after_first = read_one_lenient(&value, skip_ws(value.as_bytes(), 0), &mut params);
        if let Ok((rest, _)) = read_params(&value, after_first, ParamReading::Lenient, None) {
            params.0.extend(rest.0);
        }
        params
    }

    /// The first parameter named `name`, case-insensitively.
    pub fn get(&self, name: &str) -> Option<&ParamValue> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }

    /// The text of `name`, or `None` when absent or a bare flag.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(ParamValue::as_str)
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SipStr, &ParamValue)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    /// Append a parameter, keeping any same-named one — the shape the wire had.
    pub fn push(&mut self, name: impl Into<SipStr>, value: ParamValue) {
        self.0.push((name.into(), value));
    }

    /// Set `name`, replacing the first occurrence in place (so its wire
    /// position survives) or appending when absent.
    pub fn set(&mut self, name: impl Into<SipStr>, value: ParamValue) {
        let name: SipStr = name.into();
        match self.0.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&name)) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name, value)),
        }
    }

    /// Drop every parameter named `name`.
    pub fn remove(&mut self, name: &str) {
        self.0.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    /// [`set`](Self::set) as a functional update.
    pub fn with(mut self, name: impl Into<SipStr>, value: ParamValue) -> Self {
        self.set(name, value);
        self
    }

    /// [`remove`](Self::remove) as a functional update.
    pub fn without(mut self, name: &str) -> Self {
        self.remove(name);
        self
    }

    /// Append `;name[=value]` for each parameter.
    pub fn render(&self, out: &mut Wire) {
        for (name, value) in &self.0 {
            out.byte(b';');
            out.str(name.as_str());
            value.render(out);
        }
    }

    /// Append `name[=value]` for each parameter, comma-separated — the
    /// credentials layout (RFC 3261 §20.7).
    pub fn render_comma_separated(&self, out: &mut Wire) {
        for (i, (name, value)) in self.0.iter().enumerate() {
            if i > 0 {
                out.str(", ");
            }
            out.str(name.as_str());
            value.render(out);
        }
    }
}

impl FromIterator<(SipStr, ParamValue)> for Params {
    fn from_iter<I: IntoIterator<Item = (SipStr, ParamValue)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// How a parameter list meets a byte its grammar does not admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamReading {
    /// The list is refused: the reading of wire bytes.
    Strict,
    /// The parameters before it are kept and the list ends there: the reading
    /// of a value whose leading part means something whatever follows it
    /// (a Reason's cause).
    UpToFirstBad,
    /// It is skipped and the list reads on, every parameter kept as written,
    /// repeats included: the reading of a message held to no wire grammar
    /// (ADR-0053 §6). It never refuses.
    Lenient,
}

/// A parameter whose value the grammar also admits as an unbracketed
/// `IPv6address` (RFC 3261 §25.1 `via-received`), beside `gen-value`.
pub(crate) type Ipv6Valued = Option<&'static str>;

/// Read `*( SEMI generic-param )` from byte `i` of `base` (RFC 3261 §25.1):
/// `generic-param = token [ EQUAL gen-value ]`, `gen-value = token / host /
/// quoted-string`, LWS allowed around `;` and `=`. A parameter name appears
/// once (§7.3.1). Yields the list and the position after it: the end of
/// `base` in a [`ParamReading::Strict`] read, which refuses anything else,
/// where the list stopped in the other two.
pub(crate) fn read_params(
    base: &SipStr,
    i: usize,
    reading: ParamReading,
    ipv6_valued: Ipv6Valued,
) -> Result<(Params, usize), SipParseError> {
    let bytes = base.as_bytes();
    let mut params = Params::new();
    let mut i = i;
    loop {
        let at = skip_ws(bytes, i);
        match byte_at(bytes, at) {
            None => return long_list_repeat(params, at, reading),
            Some(b';') => {}
            Some(_) => match reading {
                ParamReading::Strict => {
                    return Err(SipParseError::new(format!(
                        "unexpected {:?} where a `;` parameter or the end belongs",
                        clamped(base.as_str(), at, bytes.len())
                    )))
                }
                ParamReading::UpToFirstBad => return long_list_repeat(params, i, reading),
                ParamReading::Lenient => {
                    i = scan_until(bytes, at, b";");
                    continue;
                }
            },
        }
        if reading == ParamReading::Lenient {
            i = read_one_lenient(base, skip_ws(bytes, at + 1), &mut params);
            continue;
        }
        match read_generic_param(base, skip_ws(bytes, at + 1), ipv6_valued) {
            // A short list is checked as it is read, a long one once at its
            // end, so a list of thousands of parameters stays linear.
            Ok((name, _, _)) if params.len() < SHORT_LIST && params.has(name.as_str()) => {
                if reading == ParamReading::Strict {
                    return Err(repeated(name.as_str()));
                }
                return Ok((params, i));
            }
            Ok((name, value, end)) => {
                params.push(name, value);
                i = end;
            }
            Err(refusal) if reading == ParamReading::Strict => return Err(refusal),
            Err(_) => return long_list_repeat(params, i, reading),
        }
    }
}

/// The end of [`read_params`] for a list longer than [`SHORT_LIST`], whose
/// repeats were not checked as it was read: a strict read refuses one, any
/// other keeps the parameters before it.
fn long_list_repeat(
    mut params: Params,
    end: usize,
    reading: ParamReading,
) -> Result<(Params, usize), SipParseError> {
    if params.len() <= SHORT_LIST || reading == ParamReading::Lenient {
        return Ok((params, end));
    }
    match first_repeat(&params) {
        None => Ok((params, end)),
        Some(at) if reading == ParamReading::Strict => {
            Err(repeated(params.0.get(at).map_or("", |(name, _)| name.as_str())))
        }
        Some(at) => {
            params.0.truncate(at);
            Ok((params, end))
        }
    }
}

/// A list this long is checked for repeats by hashing, not by a scan.
const SHORT_LIST: usize = 16;

/// The refusal of a parameter name that appears twice (RFC 3261 §7.3.1).
pub(crate) fn repeated(name: &str) -> SipParseError {
    SipParseError::new(format!(
        "repeated parameter {name:?}: a parameter name appears once (RFC 3261 §7.3.1)"
    ))
}

/// A name compared and hashed without case, as parameter names compare.
struct Folded<'a>(&'a str);

impl PartialEq for Folded<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(other.0)
    }
}

impl Eq for Folded<'_> {}

impl std::hash::Hash for Folded<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for b in self.0.bytes() {
            state.write_u8(b.to_ascii_lowercase());
        }
        state.write_u8(0xff);
    }
}

/// The index of the first parameter of `params` whose name an earlier one
/// already holds, in one pass.
pub(crate) fn first_repeat(params: &Params) -> Option<usize> {
    let mut seen = lookup_map::LookupSet::new();
    params.iter().position(|(name, _)| !seen.insert(Folded(name.as_str())))
}

/// One `generic-param` at byte `i`: its name, value and the position after it.
fn read_generic_param(
    base: &SipStr,
    i: usize,
    ipv6_valued: Ipv6Valued,
) -> Result<(SipStr, ParamValue, usize), SipParseError> {
    let bytes = base.as_bytes();
    let text = base.as_str();
    let name_end = token_end(bytes, i);
    let after_name = skip_ws(bytes, name_end);
    if name_end == i || !matches!(byte_at(bytes, after_name), None | Some(b'=' | b';')) {
        return Err(SipParseError::new(format!(
            "parameter name is not a token: {:?}",
            clamped(text, i, bytes.len())
        )));
    }
    let name = sub(base, i, name_end);
    if byte_at(bytes, after_name) != Some(b'=') {
        return Ok((name, ParamValue::Flag, name_end));
    }
    let value_at = skip_ws(bytes, after_name + 1);
    let (value, end) = match byte_at(bytes, value_at) {
        Some(b'"') => {
            let end = quoted_close(bytes, value_at).ok_or_else(|| {
                SipParseError::new(format!("unterminated quoted value of parameter {name}"))
            })?;
            (ParamValue::Quoted(read_quoted(base, value_at).0), end)
        }
        Some(b'[') => {
            let close = index_of(bytes, b']', value_at).ok_or_else(|| {
                SipParseError::new(format!("unclosed IPv6 reference in parameter {name}"))
            })?;
            let inner = clamped(text, value_at + 1, close);
            if ipv6_address(inner).is_none() {
                return Err(SipParseError::new(format!(
                    "parameter {name}: invalid IPv6 reference [{inner}]"
                )));
            }
            (ParamValue::Token(sub(base, value_at, close + 1)), close + 1)
        }
        _ => {
            let end = token_end(bytes, value_at);
            let ipv6 = ipv6_valued.is_some_and(|n| name.eq_ignore_ascii_case(n))
                && byte_at(bytes, end) == Some(b':');
            let end = if ipv6 { scan_until(bytes, value_at, b"; \t,") } else { end };
            if ipv6 && ipv6_address(clamped(text, value_at, end)).is_none() {
                return Err(SipParseError::new(format!(
                    "value of parameter {name} is not an IPv6address (RFC 5954 §4.1)"
                )));
            }
            if end == value_at {
                return Err(SipParseError::new(format!(
                    "value of parameter {name} is not a token, host or quoted-string"
                )));
            }
            (ParamValue::Token(sub(base, value_at, end)), end)
        }
    };
    Ok((name, value, end))
}

/// `auth-param *( COMMA auth-param )` from byte `i` to the end of `base`
/// (RFC 3261 §25.1): `auth-param = token EQUAL ( token / quoted-string )`,
/// each name once (§7.3.1), refused at the first byte off the grammar.
pub(crate) fn read_auth_params(base: &SipStr, mut i: usize) -> Result<Params, SipParseError> {
    let bytes = base.as_bytes();
    let off = |what: &str| SipParseError::new(format!("a malformed auth-param: {what}"));
    let mut params = Params::new();
    loop {
        let name_end = token_end(bytes, i);
        if name_end == i {
            return Err(off("no parameter name"));
        }
        let name = sub(base, i, name_end);
        i = skip_ws(bytes, name_end);
        if byte_at(bytes, i) != Some(b'=') {
            return Err(off(&format!("{name} has no value")));
        }
        i = skip_ws(bytes, i + 1);
        let value = if byte_at(bytes, i) == Some(b'"') {
            let close = quoted_close(bytes, i).ok_or_else(|| off(&format!("{name}'s quote")))?;
            let (text, _) = read_quoted(base, i);
            i = close;
            ParamValue::Quoted(text)
        } else {
            let value_end = token_end(bytes, i);
            if value_end == i {
                return Err(off(&format!("{name}'s value is not a token")));
            }
            let token = sub(base, i, value_end);
            i = value_end;
            ParamValue::Token(token)
        };
        if params.len() < SHORT_LIST && params.has(name.as_str()) {
            return Err(repeated(name.as_str()));
        }
        params.push(name, value);
        i = skip_ws(bytes, i);
        match byte_at(bytes, i) {
            None => return long_list_repeat(params, i, ParamReading::Strict).map(|(p, _)| p),
            Some(b',') => i = skip_ws(bytes, i + 1),
            Some(_) => return Err(off("no comma between parameters")),
        }
    }
}

/// Read one `name[=value]` at byte `i` the way [`ParamReading::Lenient`]
/// reads it; yields the position after it.
fn read_one_lenient(base: &SipStr, mut i: usize, params: &mut Params) -> usize {
    let bytes = base.as_bytes();

    let name_end = scan_until(bytes, i, b"=; \t,>");
    let name = sub(base, i, name_end);
    i = skip_ws(bytes, name_end);

    if byte_at(bytes, i) == Some(b'=') {
        i = skip_ws(bytes, i + 1);
        if byte_at(bytes, i) == Some(b'"') {
            let (text, end) = read_quoted(base, i);
            params.push(name, ParamValue::Quoted(text));
            return end;
        }
        let value_end = scan_until(bytes, i, b";, \t>");
        params.push(name, ParamValue::Token(sub(base, i, value_end)));
        return value_end;
    }
    if !name.is_empty() {
        params.push(name, ParamValue::Flag);
    }
    i
}

#[cfg(test)]
mod tests;
