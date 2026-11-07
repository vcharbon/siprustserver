//! The parameters of a URI (RFC 3261 §25.1 `uri-parameters`, §19.1.1): each
//! part held to its class, and a name once.

use crate::char_class::is_token_char;
use crate::sip_str::SipStr;

use super::super::params::{ParamValue, Params};
use super::super::scan::{byte_at, clamped, scan_until, sub};
use super::read::{maddr_refusal, paramchar_refusal, MADDR};
use super::UriRole;

/// The `uri-parameters` from byte `j` of a sip or sips URI (RFC 3261 §25.1),
/// each `;name[=value]`, the parts held to their classes when `role` is given:
/// a name to `paramchar`, a `transport`, `user` or `method` value to `token`
/// (§25.1 gives each a token form), any other value to `paramchar`, and a
/// `maddr` of a [`UriRole::Target`] to `host`. A name appears once
/// (§19.1.1): a second copy of an identical value is collapsed into the first,
/// one of another value refused. Yields the list, the collapsed copies and the
/// position after the list, a `?` or the end.
pub(super) fn read_uri_params(
    value: &SipStr,
    mut j: usize,
    role: Option<UriRole>,
) -> Result<(Params, Collapsed, usize), String> {
    let bytes = value.as_bytes();
    let text = value.as_str();
    let mut params = Params::new();
    let mut collapsed = Collapsed::default();
    let mut names = NameIndex::default();
    while byte_at(bytes, j) == Some(b';') {
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
                if TOKEN_VALUED.iter().any(|t| name.eq_ignore_ascii_case(t)) {
                    token_refusal("uri-parameter value", read)?;
                } else {
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
        match names.find(&params, &name) {
            None => {
                names.add(&params, &name);
                params.push(name, param);
            }
            Some(first) if params.iter().nth(first).is_some_and(|(_, v)| *v == param) => {
                collapsed.names.push(name);
                collapsed.segments.push(segment_at..j);
            }
            Some(_) => {
                return Err(format!(
                    "uri-parameter {name:?} repeated with another value (RFC 3261 §19.1.1)"
                ))
            }
        }
    }
    Ok((params, collapsed, j))
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
fn token_refusal(what: &str, run: &str) -> Result<(), String> {
    if run.is_empty() {
        return Err(format!("empty {what}"));
    }
    match run.bytes().find(|&b| !is_token_char(b)) {
        None => Ok(()),
        Some(b) => Err(format!("{what}: byte 0x{b:02x} is no token character in {run:?}")),
    }
}
