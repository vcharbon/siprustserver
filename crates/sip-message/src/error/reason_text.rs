//! How a refusal reason names what it read, so its key ([`super::reason_key`])
//! keeps the shape and drops the value: a known header by its canonical name,
//! every name or value the sender chose as a debug string.

use std::fmt;

use crate::header::HeaderName;

/// `name` as a reason writes it: a known header's canonical name, an extension
/// header's name as written, debug-quoted. Formatted only when a reason is.
pub(crate) fn header(name: &HeaderName) -> Header<'_> {
    Header(name)
}

/// A header name as a reason writes it ([`header`]).
pub(crate) struct Header<'a>(&'a HeaderName);

impl fmt::Display for Header<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            HeaderName::Other(written) => write!(f, "{written:?}"),
            known => f.write_str(known.as_wire_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::header;
    use crate::header::HeaderName;
    use crate::sip_str::SipStr;

    #[test]
    fn a_known_header_is_named_canonically_an_extension_is_quoted() {
        let lowercase = HeaderName::of(&SipStr::owned("content-length"));
        assert_eq!(header(&lowercase).to_string(), "Content-Length");
        let extension = HeaderName::of(&SipStr::owned("X-Fakey-McFakename"));
        assert_eq!(header(&extension).to_string(), "\"X-Fakey-McFakename\"");
    }
}
