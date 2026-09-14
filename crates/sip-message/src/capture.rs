//! Header capture: the values of a configured list of header names, read off
//! a parsed message (or the header lines of one) once, as owned text — the
//! seam a record keeps a message's chosen headers through, so no reader of
//! the record re-parses the datagram.

use crate::header::HeaderName;
use crate::types::{SipHeader, SipMessage, SipRequest, SipResponse};

/// Every value of every header in `names`, as `(canonical name, value)` pairs:
/// the names in the order given, and under each name its header lines in wire
/// order. A name the message does not carry contributes nothing; a header
/// line's value is kept as written, comma folds included. Name matching is
/// casing- and compact-form-insensitive.
pub fn captured_headers(headers: &[SipHeader], names: &[HeaderName]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in names {
        let canonical = name.as_wire_str();
        for line in headers.iter().filter(|h| name.matches(&h.name)) {
            out.push((canonical.to_string(), line.value.as_str().to_string()));
        }
    }
    out
}

macro_rules! capture_surface {
    ($t:ty) => {
        impl $t {
            /// The message's values of `names` — see [`captured_headers`].
            pub fn captured_headers(&self, names: &[HeaderName]) -> Vec<(String, String)> {
                captured_headers(self.headers(), names)
            }
        }
    };
}

capture_surface!(SipRequest);
capture_surface!(SipResponse);
capture_surface!(SipMessage);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip_str::SipStr;

    fn line(name: &str, value: &str) -> SipHeader {
        SipHeader { name: SipStr::owned(name), value: SipStr::owned(value) }
    }

    #[test]
    fn names_in_configured_order_then_lines_in_wire_order() {
        let headers = [
            line("Privacy", "id"),
            line("Allow", "INVITE, ACK"),
            line("Via", "SIP/2.0/UDP host"),
            line("allow", "BYE"),
        ];
        let names = [HeaderName::Allow, HeaderName::Privacy, HeaderName::Accept];
        assert_eq!(
            captured_headers(&headers, &names),
            vec![
                ("Allow".to_string(), "INVITE, ACK".to_string()),
                ("Allow".to_string(), "BYE".to_string()),
                ("Privacy".to_string(), "id".to_string()),
            ]
        );
    }

    #[test]
    fn an_extension_name_matches_its_own_spelling_only() {
        let headers = [line("X-Custom", "one"), line("x-custom", "two"), line("X-Other", "no")];
        let names = [HeaderName::from("X-Custom")];
        assert_eq!(
            captured_headers(&headers, &names),
            vec![
                ("X-Custom".to_string(), "one".to_string()),
                ("X-Custom".to_string(), "two".to_string()),
            ]
        );
    }

    #[test]
    fn no_names_captures_nothing() {
        let headers = [line("Allow", "INVITE")];
        assert!(captured_headers(&headers, &[]).is_empty());
    }
}
