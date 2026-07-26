//! [`Params`] — an ordered parameter list, and [`ParamValue`] — what one
//! parameter carries.
//!
//! Wire order round-trips faithfully because the list *is* the wire order, and
//! the common one-or-two-parameter case needs no heap node. Names keep their
//! wire spelling; every lookup is case-insensitive, which is what RFC 3261
//! §7.3.1 actually specifies.

use smallvec::SmallVec;

use crate::sip_str::SipStr;

use super::scan::{read_quoted, scan_until, skip_ws, sub};
use super::wire::Wire;

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
        if v.is_empty() || v.bytes().any(|b| matches!(b, b';' | b',' | b'"' | b'>' | b' ' | b'\t' | b'=' | b'?'))
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

/// Which bytes end a parameter name and a bare parameter value — the one
/// difference between the parameter lists of the three grammars that carry
/// them.
pub(crate) struct ParamStyle {
    name_delims: &'static [u8],
    value_delims: &'static [u8],
}

/// Header-level parameters: everything after `>` or the addr-spec.
pub(crate) const HEADER_PARAMS: ParamStyle =
    ParamStyle { name_delims: b"=; \t,>", value_delims: b";, \t>" };

/// URI parameters: `?` opens the embedded headers, so it ends a value too.
pub(crate) const URI_PARAMS: ParamStyle =
    ParamStyle { name_delims: b"=;>? \t", value_delims: b";>? \t" };

/// Read `;`-separated parameters from byte `i`; yields the list and the
/// position after the last one.
pub(crate) fn parse_semicolon_params(
    base: &SipStr,
    mut i: usize,
    style: &ParamStyle,
) -> (Params, usize) {
    let bytes = base.as_bytes();
    let mut params = Params::new();
    loop {
        let at = skip_ws(bytes, i);
        if at >= bytes.len() || bytes[at] != b';' {
            return (params, i);
        }
        i = read_one(base, skip_ws(bytes, at + 1), style, &mut params);
    }
}

/// Read comma-separated `name=value` parameters covering all of `base` from
/// byte `i` — the credentials layout, where `,` separates and `;` is data.
pub(crate) fn parse_comma_params(base: &SipStr, mut i: usize) -> Params {
    const STYLE: ParamStyle = ParamStyle { name_delims: b"=, \t", value_delims: b"," };
    let bytes = base.as_bytes();
    let mut params = Params::new();
    loop {
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            return params;
        }
        i = read_one(base, i, &STYLE, &mut params);
        i = skip_ws(bytes, i);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        } else {
            return params;
        }
    }
}

/// Read one `name[=value]` at byte `i`; yields the position after it.
fn read_one(base: &SipStr, mut i: usize, style: &ParamStyle, params: &mut Params) -> usize {
    let bytes = base.as_bytes();
    let len = bytes.len();

    let name_end = scan_until(bytes, i, style.name_delims);
    let name = sub(base, i, name_end);
    i = skip_ws(bytes, name_end);

    if i < len && bytes[i] == b'=' {
        i = skip_ws(bytes, i + 1);
        if i < len && bytes[i] == b'"' {
            let (text, end) = read_quoted(base, i);
            params.push(name, ParamValue::Quoted(text));
            return end;
        }
        let value_end = scan_until(bytes, i, style.value_delims);
        params.push(name, ParamValue::Token(sub(base, i, value_end)));
        return value_end;
    }
    if !name.is_empty() {
        params.push(name, ParamValue::Flag);
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_params(v: &str) -> Params {
        parse_semicolon_params(&SipStr::owned(v), 0, &HEADER_PARAMS).0
    }

    #[test]
    fn wire_order_and_spelling_survive() {
        let p = header_params(";Tag=abc;LR;q=0.5");
        let names: Vec<&str> = p.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Tag", "LR", "q"]);
        assert_eq!(p.value("tag"), Some("abc"));
        assert!(p.get("lr").unwrap().is_flag());
    }

    #[test]
    fn a_flag_is_distinct_from_an_empty_value() {
        let p = header_params(";lr;maddr=");
        assert_eq!(p.get("lr"), Some(&ParamValue::Flag));
        assert_eq!(p.get("maddr"), Some(&ParamValue::Token(SipStr::EMPTY)));
    }

    #[test]
    fn a_quoted_value_keeps_its_separators() {
        let p = header_params(r#";reason="a;b,c";next=1"#);
        assert_eq!(p.value("reason"), Some("a;b,c"));
        assert_eq!(p.value("next"), Some("1"));
        let mut w = Wire::new();
        p.render(&mut w);
        assert_eq!(w.as_str(), r#";reason="a;b,c";next=1"#);
    }

    #[test]
    fn set_replaces_in_place_and_append_keeps_position() {
        let mut p = header_params(";branch=z1;rport");
        p.set("BRANCH", ParamValue::Token(SipStr::owned("z2")));
        p.set("received", ParamValue::Token(SipStr::owned("1.2.3.4")));
        let mut w = Wire::new();
        p.render(&mut w);
        assert_eq!(w.as_str(), ";branch=z2;rport;received=1.2.3.4");
    }

    #[test]
    fn credentials_parameters_split_on_commas_not_semicolons() {
        let p = parse_comma_params(&SipStr::owned(r#"realm="a;b", nonce=xyz, stale=FALSE"#), 0);
        assert_eq!(p.len(), 3);
        assert_eq!(p.value("realm"), Some("a;b"));
        assert_eq!(p.value("nonce"), Some("xyz"));
    }
}
