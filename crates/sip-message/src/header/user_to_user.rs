//! [`UserToUser`] — one UUI data element (RFC 7433 §4.1).
//!
//! `UUI = "User-to-User" HCOLON uui-value *(COMMA uui-value)`, `uui-value =
//! uui-data *(SEMI uui-param)`, `uui-data = token / quoted-string`. A message
//! may carry several lines, each several comma-separated values, and a
//! receiver reads every one (§4.1), so the value is [`Folding::Comma`] and a
//! reader takes the list.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::cardinality::{held_to_the_table, lines};
use super::name::HeaderId;
use super::params::{read_params, ParamReading, ParamValue, Params};
use super::scan::{byte_at, quoted_close, read_quoted, sub, token_end};
use super::single_valued::Listed;
use super::value::HeaderValue;
use super::wire::Wire;

/// One UUI data element and its parameters (`purpose`, `content`,
/// `encoding`, any other).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserToUser {
    data: ParamValue,
    params: Params,
}

impl UserToUser {
    /// The UUI data as written: a token, or a quoted-string's text unescaped.
    pub fn data(&self) -> &str {
        self.data.as_str().unwrap_or_default()
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.params.get(name)
    }
}

impl HeaderValue for UserToUser {
    type Lines = lines::Comma;
    held_to_the_table!();

    const HEADER: Listed = Listed::Known(HeaderId::UserToUser);

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let refused =
            |what: &str| SipParseError::new(format!("User-to-User {what}: {:?}", value.as_str()));
        let (data, end) = match byte_at(bytes, 0) {
            Some(b'"') => {
                let end = quoted_close(bytes, 0).ok_or_else(|| refused("has an open quote"))?;
                (ParamValue::Quoted(read_quoted(&value, 0).0), end)
            }
            _ => {
                let end = token_end(bytes, 0);
                if end == 0 {
                    return Err(refused("has no token or quoted-string"));
                }
                (ParamValue::Token(sub(&value, 0, end)), end)
            }
        };
        let (params, _) =
            read_params(&value, end, ParamReading::Strict, None).map_err(|e| refused(&e.reason))?;
        Ok(Self { data, params })
    }

    fn render(&self, out: &mut Wire) {
        match &self.data {
            ParamValue::Quoted(text) => out.quoted(text.as_str()),
            other => out.str(other.as_str().unwrap_or_default()),
        }
        self.params.render(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line and every comma-separated value reads, each with its
    /// parameters.
    #[test]
    fn every_value_of_every_line_reads() {
        let lines = ["0431;encoding=hex;purpose=a", "0432;encoding=hex, \"x, y\";purpose=b"];
        let mut values = Vec::new();
        for line in lines {
            UserToUser::parse_line_into(&SipStr::owned(line), &mut values).unwrap();
        }
        let data: Vec<&str> = values.iter().map(UserToUser::data).collect();
        assert_eq!(data, ["0431", "0432", "x, y"]);
        assert_eq!(values[2].param("purpose").and_then(ParamValue::as_str), Some("b"));
        assert_eq!(values[2].to_wire(), "\"x, y\";purpose=b");
    }

    #[test]
    fn a_value_off_the_grammar_is_refused() {
        for raw in ["", "a b", "\"open", "0431;encoding=hex junk", "0431;a=1;a=2"] {
            assert!(UserToUser::parse(&SipStr::owned(raw)).is_err(), "{raw:?}");
        }
    }
}
