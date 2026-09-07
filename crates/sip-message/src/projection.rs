//! [`HeaderProjection`] — a message's header list kept as owned pairs, read by
//! header IDENTITY after the message itself is gone.
//!
//! A reader that keeps `(name, value)` pairs and probes them with string
//! comparison re-implements name resolution and misses compact spellings. This
//! type is the one projection such a reader holds: every lookup resolves
//! through [`HeaderName`], so `v:` and `Via` name the same header everywhere.

use crate::header::HeaderName;
use crate::types::SipMessage;

/// A message's headers as owned `(name, value)` pairs, wire order and wire
/// spellings kept. Lookup is by header identity — casing- and
/// compact-form-insensitive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderProjection(Vec<(String, String)>);

impl HeaderProjection {
    /// Project `msg`'s header list.
    pub fn of(msg: &SipMessage) -> Self {
        HeaderProjection(
            msg.headers().iter().map(|h| (h.name.to_string(), h.value.to_string())).collect(),
        )
    }

    /// The value of `name`, FIRST occurrence in wire order — wire order is what
    /// a recording preserves, so "the header" is the first one the message
    /// spelled.
    pub fn first(&self, name: &str) -> Option<&str> {
        let want = HeaderName::from(name);
        self.0.iter().find(|(n, _)| want.matches(n)).map(|(_, v)| v.as_str())
    }

    /// Every value of `name`, in wire order.
    pub fn all(&self, name: &str) -> Vec<&str> {
        let want = HeaderName::from(name);
        self.0.iter().filter(|(n, _)| want.matches(n)).map(|(_, v)| v.as_str()).collect()
    }
}

/// Pairs a test states directly stand in for a projected message.
impl From<Vec<(String, String)>> for HeaderProjection {
    fn from(headers: Vec<(String, String)>) -> Self {
        HeaderProjection(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::SipParser;
    use crate::CustomParser;

    fn projected() -> HeaderProjection {
        let msg = CustomParser::new()
            .parse(
                b"INVITE sip:b@h SIP/2.0\r\n\
v: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
From: <sip:a@h>;tag=f1\r\n\
To: <sip:b@h>\r\n\
Call-ID: proj-1\r\n\
CSeq: 1 INVITE\r\n\
c: application/sdp\r\n\
X-Api-Call: call-9\r\n\
x-api-call: call-10\r\n\
l: 3\r\n\r\nv=0",
            )
            .expect("test message parses");
        HeaderProjection::of(&msg)
    }

    /// The live gap this type closes: a lookup by either spelling finds a
    /// header the message spelled the other way.
    #[test]
    fn a_lookup_resolves_identity_not_spelling() {
        let headers = projected();
        assert_eq!(headers.first("Via"), Some("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1"));
        assert_eq!(headers.first("Content-Type"), Some("application/sdp"));
        assert_eq!(headers.first("v"), headers.first("Via"));
        assert_eq!(headers.first("i"), Some("proj-1"), "a long spelling answers a compact probe");
        assert_eq!(headers.first("Content-Length"), Some("3"));
        assert_eq!(headers.first("X-Missing"), None);
    }

    /// Duplicates keep wire order: `first` is the first the message spelled,
    /// `all` is every one, and an extension name matches case-insensitively.
    #[test]
    fn duplicates_read_in_wire_order() {
        let headers = projected();
        assert_eq!(headers.first("X-Api-Call"), Some("call-9"));
        assert_eq!(headers.all("x-API-call"), ["call-9", "call-10"]);
        assert!(headers.all("Reason").is_empty());
    }
}
