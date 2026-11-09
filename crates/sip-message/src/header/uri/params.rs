//! The parameters of a URI (RFC 3261 §25.1 `uri-parameters`, §19.1.1): each
//! part held to its class, and a name once.

use std::ops::ControlFlow::{Break, Continue};

use crate::char_class::{is_token_char, is_unreserved};
use crate::cursor::{items, walk};
use crate::sip_str::SipStr;

use super::super::params::{ParamValue, Params};
use super::super::scan::{byte_at, clamped, scan_until, sub};
use super::read::{maddr_refusal, paramchar_refusal, MADDR};
use super::UriRole;

/// The `uri-parameters` from byte `j` of a sip or sips URI (RFC 3261 §25.1),
/// each `;name[=value]`, the parts held to their classes when `role` is given:
/// a name to `paramchar`, a value to `paramchar`, a `transport`, `user` or
/// `method` value to `token` as well (§25.1 gives each a token form beside the
/// `other-param` one), and a `maddr` of a [`UriRole::Target`] to `host`. A name
/// appears once (§19.1.1): a second copy of a value equal as §19.1.4 compares
/// is collapsed into the first, one of another value refused. Yields the list,
/// the collapsed copies and the position after the list, a `?` or the end.
pub(super) fn read_uri_params(
    value: &SipStr,
    j: usize,
    role: Option<UriRole>,
) -> Result<(Params, Collapsed, usize), String> {
    let mut list = List::default();
    let read = walk(value.as_bytes(), j, |j| match list.segment(value, j, role) {
        Ok(Some(next)) => Continue(next),
        Ok(None) => Break(Ok(j)),
        Err(refusal) => Break(Err(refusal)),
    });
    let end = read.map_err(|stall| stall.to_string())??;
    Ok((list.params, list.collapsed, end))
}

/// The parameters read so far.
#[derive(Default)]
struct List {
    params: Params,
    collapsed: Collapsed,
    names: NameIndex,
}

impl List {
    /// Reads the `;name[=value]` at byte `j`; the position after it, or `None`
    /// when no `;` opens one there.
    fn segment(
        &mut self,
        value: &SipStr,
        mut j: usize,
        role: Option<UriRole>,
    ) -> Result<Option<usize>, String> {
        let bytes = value.as_bytes();
        let text = value.as_str();
        let List { params, collapsed, names } = self;
        if byte_at(bytes, j) != Some(b';') {
            return Ok(None);
        }
        let segment_at = j;
        let name_at = j + 1;
        let name_end = scan_until(bytes, name_at, b"=;?");
        let name = sub(value, name_at, name_end);
        if role.is_some() {
            paramchar_refusal("uri-parameter name", &name)?;
        }
        let param = if byte_at(bytes, name_end) == Some(b'=') {
            j = scan_until(bytes, name_end + 1, b";?");
            let read = clamped(text, name_end + 1, j);
            if role.is_some() {
                let token_valued = TOKEN_VALUED.iter().any(|t| name.eq_ignore_ascii_case(t));
                if !(token_valued && is_token(read)) {
                    paramchar_refusal("uri-parameter value", read)?;
                }
            }
            if role == Some(UriRole::Target) && name.eq_ignore_ascii_case(MADDR) {
                maddr_refusal(read).map_or(Ok(()), |r| Err(format!("maddr: {r}")))?;
            }
            ParamValue::Token(sub(value, name_end + 1, j))
        } else {
            if role == Some(UriRole::Target) && name.eq_ignore_ascii_case(MADDR) {
                return Err("maddr names no host".to_string());
            }
            j = name_end;
            ParamValue::Flag
        };
        match names.find(params, &name) {
            None => {
                names.add(params, &name);
                params.push(name, param);
            }
            Some(first) if params.iter().nth(first).is_some_and(|(_, v)| same_value(v, &param)) => {
                collapsed.names.push(name);
                collapsed.segments.push(segment_at..j);
            }
            Some(_) => {
                return Err(format!(
                    "uri-parameter {name:?} repeated with another value (RFC 3261 §19.1.1)"
                ))
            }
        }
        Ok(Some(j))
    }
}

/// The second, identical copies a URI read collapsed: their names, and the
/// byte ranges of their `;name[=value]` text.
#[derive(Default)]
pub(super) struct Collapsed {
    pub(super) names: Vec<SipStr>,
    pub(super) segments: Vec<std::ops::Range<usize>>,
}

/// The names a parameter list holds, found by a scan of the list while it is
/// short and by hashing past that, so a URI of thousands stays linear.
#[derive(Default)]
struct NameIndex(Option<lookup_map::LookupMap<String, usize>>);

/// A list this long is indexed by hashing, not scanned.
const SHORT_LIST: usize = 16;

impl NameIndex {
    /// The position in `params` of the parameter named `name`, case aside.
    fn find(&self, params: &Params, name: &str) -> Option<usize> {
        match &self.0 {
            Some(index) => index.get(&name.to_ascii_lowercase()).copied(),
            None => params.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)),
        }
    }

    /// Record that `name` is about to be pushed onto `params`.
    fn add(&mut self, params: &Params, name: &str) {
        if self.0.is_none() && params.len() >= SHORT_LIST {
            let index = params.iter().enumerate().map(|(i, (n, _))| (n.to_ascii_lowercase(), i));
            self.0 = Some(index.collect());
        }
        if let Some(index) = &mut self.0 {
            index.insert(name.to_ascii_lowercase(), params.len());
        }
    }
}

/// The URI parameters whose value §25.1 gives a `token` form.
const TOKEN_VALUED: &[&str] = &["transport", "user", "method"];

/// `token`.
fn is_token(run: &str) -> bool {
    !run.is_empty() && run.bytes().all(is_token_char)
}

/// Whether two values of one URI parameter are equal as RFC 3261 §19.1.4
/// compares them: case aside, an escaped unreserved character equal to the
/// character itself.
fn same_value(a: &ParamValue, b: &ParamValue) -> bool {
    match (a.as_str(), b.as_str()) {
        (None, None) => true,
        (Some(a), Some(b)) => unescaped(a).eq(unescaped(b)),
        _ => false,
    }
}

/// The bytes of `run`, lower-cased, each `%HH` of an unreserved character
/// read as that character (RFC 3261 §19.1.4, RFC 2396 §2.3).
fn unescaped(run: &str) -> impl Iterator<Item = u8> + '_ {
    let bytes = run.as_bytes();
    items(bytes, move |i| {
        let b = *bytes.get(i)?;
        let decoded = (b == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            .filter(|&c| is_unreserved(c));
        Some(match decoded {
            Some(c) => (c.to_ascii_lowercase(), i + 3),
            None => (b.to_ascii_lowercase(), i + 1),
        })
    })
}
