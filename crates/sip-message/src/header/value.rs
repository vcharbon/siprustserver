//! [`HeaderValue`] — the one trait every structured header value implements.
//!
//! Parse and render sit on the same type, so a value is its own builder and
//! there is no second shape to keep aligned. [`Folding`] states, per header,
//! how several values of it may be laid out on the wire — the fact a
//! comma-splitting reader needs and a string literal cannot carry.

use crate::error::SipParseError;
use crate::parser::custom::structured_headers::top_level_comma_entries;
use crate::sip_str::SipStr;

use super::name::HeaderName;
use super::wire::Wire;

/// How the wire may lay out several values of one header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folding {
    /// At most one value per message (From, To, CSeq, Call-ID, Max-Forwards).
    Single,
    /// Several values, which may share one line comma-separated — RFC 3261
    /// §7.3.1 (Via, Route, Contact, P-Asserted-Identity).
    Comma,
    /// One value per line, and a comma inside a line is DATA: the credentials
    /// family (RFC 3261 §20.7) carries its parameters comma-separated inside a
    /// single value, so splitting a line tears one value in half.
    Opaque,
    /// One value per line, and the value is itself the comma-separated token
    /// set the value type parses (Require, Supported, Allow): a line reads as
    /// one set rather than as several values, and several lines union into one.
    SetPerLine,
}

/// A structured header value.
pub trait HeaderValue: Sized + Clone + std::fmt::Debug + Send + Sync + 'static {
    /// The header this value belongs to.
    fn header_name() -> HeaderName;

    /// How the wire may lay out several of these.
    fn folding() -> Folding;

    /// Read one value — one comma-separated entry for a [`Folding::Comma`]
    /// header, the whole line otherwise.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError>;

    /// Append the wire form to the build buffer.
    fn render(&self, out: &mut Wire);

    /// This value's header name.
    fn name(&self) -> HeaderName {
        Self::header_name()
    }

    /// Every value carried on one header line.
    fn parse_line(raw: &SipStr) -> Result<Vec<Self>, SipParseError> {
        match Self::folding() {
            Folding::Comma => {
                // Sized for the one-value line the wire almost always carries:
                // a value type is a wide struct, so a growth step costs real
                // bytes on the relay path.
                let mut values = Vec::with_capacity(1);
                for entry in top_level_comma_entries(raw.as_str()) {
                    values.push(Self::parse(&raw.reslice(entry))?);
                }
                Ok(values)
            }
            Folding::Single | Folding::Opaque | Folding::SetPerLine => Ok(vec![Self::parse(raw)?]),
        }
    }

    /// The single logical value a reader sees when the message carries several
    /// lines of this header. Set-like headers union; everything else is the
    /// first line, which is the value RFC 3261 gives meaning to.
    fn combine(values: Vec<Self>) -> Option<Self> {
        values.into_iter().next()
    }

    /// The wire form on its own — for logging and for the escape hatches that
    /// still take a string.
    fn to_wire(&self) -> String {
        let mut w = Wire::new();
        self.render(&mut w);
        w.as_str().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::Via;

    #[test]
    fn a_comma_folded_line_yields_every_value() {
        let raw = SipStr::owned("SIP/2.0/UDP a.example:5060;branch=z1, SIP/2.0/TCP b.example");
        let vias = Via::parse_line(&raw).expect("both entries parse");
        assert_eq!(vias.len(), 2);
        assert_eq!(vias[0].branch(), Some("z1"));
        assert_eq!(vias[1].host(), "b.example");
    }

    #[test]
    fn a_single_valued_header_never_splits_on_a_comma() {
        let raw = SipStr::owned("\"a,b\" <sip:a@h>;tag=1");
        let froms = crate::header::From::parse_line(&raw).expect("one value");
        assert_eq!(froms.len(), 1);
        assert_eq!(froms[0].display(), Some("a,b"));
    }
}
