//! [`HeaderValue`] — the one trait every structured header value implements.
//!
//! Parse and render sit on the same type, so a value is its own builder and
//! there is no second shape to keep aligned. Its [`Cardinality`] states, per
//! header, whether a message carries one line of it or a list, and how a list
//! lays its values out — the fact a comma-splitting reader needs and a string
//! literal cannot carry.

use crate::error::SipParseError;
use crate::parser::custom::structured_headers::top_level_comma_entries;
use crate::sip_str::SipStr;

use super::cardinality::Cardinality;
use super::name::HeaderName;
use super::single_valued::Listed;
use super::wire::Wire;

/// How the wire lays out the values of one header: the runtime echo of its
/// [`Cardinality`].
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
    /// One value per line, and the value is itself the token set the value
    /// type parses (Require, Supported, Allow, Privacy) over the separator its
    /// own grammar declares ([`HeaderName::item_separator`]): a line reads as
    /// one set rather than as several values, and several lines union into one.
    SetPerLine,
}

/// A structured header value.
pub trait HeaderValue: Sized + Clone + std::fmt::Debug + Send + Sync + 'static {
    /// How many lines of this header a message carries, as a type
    /// ([`Cardinality`]).
    type Lines: Cardinality;

    /// The header this value belongs to, which its cardinality is checked
    /// against (`header::SINGLE_VALUED`).
    const HEADER: Listed;

    /// The header this value belongs to.
    fn header_name() -> HeaderName {
        Self::HEADER.name()
    }

    /// How the wire may lay out several of these.
    fn folding() -> Folding {
        <Self::Lines as Cardinality>::FOLDING
    }

    /// Read one value — one comma-separated entry for a [`Folding::Comma`]
    /// header, the whole line otherwise.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError>;

    /// Append the wire form to the build buffer.
    fn render(&self, out: &mut Wire);

    /// This value's header name.
    fn name(&self) -> HeaderName {
        Self::header_name()
    }

    /// Whether a line holding no value reads as none: `false` where the
    /// grammar needs one (RFC 3261 §25.1), which [`parse`](Self::parse) of
    /// the empty value then refuses.
    const EMPTY_LINE_READS: bool = false;

    /// Every value carried on one header line.
    fn parse_line(raw: &SipStr) -> Result<Vec<Self>, SipParseError> {
        // Sized for the one-value line the wire almost always carries: a value
        // type is a wide struct, so a growth step costs real bytes on the
        // relay path.
        let mut values = Vec::with_capacity(1);
        Self::parse_line_into(raw, &mut values)?;
        Ok(values)
    }

    /// [`parse_line`](Self::parse_line) appending to `out`, so a reader that
    /// folds every line of a header into one list allocates it once.
    fn parse_line_into(raw: &SipStr, out: &mut Vec<Self>) -> Result<(), SipParseError> {
        read_line_into(raw, out)
    }

    /// The wire form on its own — for logging and for the escape hatches that
    /// still take a string.
    fn to_wire(&self) -> String {
        let mut w = Wire::new();
        self.render(&mut w);
        w.as_str().to_owned()
    }
}

/// [`HeaderValue::parse_line_into`] as the trait states it, for a type that
/// reads one shape of line its own way and every other line this way.
pub(crate) fn read_line_into<H: HeaderValue>(
    raw: &SipStr,
    out: &mut Vec<H>,
) -> Result<(), SipParseError> {
    match H::folding() {
        Folding::Comma => {
            let mut entries = top_level_comma_entries(raw.as_str()).peekable();
            if entries.peek().is_none() && !H::EMPTY_LINE_READS {
                out.push(H::parse(raw)?);
            }
            for entry in entries {
                out.push(H::parse(&raw.reslice(entry))?);
            }
        }
        Folding::Single | Folding::Opaque | Folding::SetPerLine => out.push(H::parse(raw)?),
    }
    Ok(())
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
