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

    /// The `n`-th item (0-based) of `name` over every row in wire order: the
    /// rows fold into one list (RFC 3261 §7.3.1), split on the header's own
    /// separator outside quoted-strings and `<…>`, each item trimmed. A header
    /// whose row is one item ([`HeaderName::one_item_per_row`]: the
    /// folding-exempt credentials family, a single-valued grammar such as
    /// Date) is counted by row, never split.
    pub fn item(&self, name: &str, n: usize) -> Option<&str> {
        let rows = self.all(name).into_iter();
        if HeaderName::one_item_per_row_of(name) {
            return rows.map(crate::whitespace::trim).nth(n);
        }
        let sep = HeaderName::item_separator_of(name);
        rows.flat_map(|row| sep.split(row)).nth(n)
    }

    /// The option tags every row of the set-like header `name` lists
    /// (`Supported`, `Require`, …), in wire order, read as
    /// [`crate::sniff::option_tags`] reads them: one case-insensitive set, a
    /// value that is no `token` dropped.
    pub fn option_tags(&self, name: &str) -> Vec<String> {
        let sep = HeaderName::item_separator_of(name);
        let mut out: Vec<String> = Vec::new();
        for row in self.all(name) {
            for tag in sep.split(row) {
                let ok = !tag.is_empty() && tag.bytes().all(crate::char_class::is_token_char);
                if ok && !out.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                    out.push(tag.to_string());
                }
            }
        }
        out
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
                b"INVITE sip:b@h SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
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

    /// An item is counted over every row, a comma-folded row and separate
    /// rows alike; a comma inside a quoted display name or `<…>` is data.
    #[test]
    fn an_item_counts_over_every_row_of_the_list() {
        let rows: HeaderProjection = vec![
            ("Diversion".to_string(), "\"a, b\" <sip:1@h>;counter=1, <sip:2@h>".to_string()),
            ("diversion".to_string(), "<sip:3@h;x=1,2>;reason=unknown".to_string()),
        ]
        .into();
        assert_eq!(rows.item("Diversion", 0), Some("\"a, b\" <sip:1@h>;counter=1"));
        assert_eq!(rows.item("Diversion", 1), Some("<sip:2@h>"));
        assert_eq!(rows.item("Diversion", 2), Some("<sip:3@h;x=1,2>;reason=unknown"));
        assert_eq!(rows.item("Diversion", 3), None);
        assert_eq!(rows.item("Reason", 0), None);
    }

    /// A row of a folding-exempt or single-valued header is one item: its
    /// commas are data (RFC 3261 §7.3.1, §20.17, §20.44).
    #[test]
    fn a_folding_exempt_or_single_valued_header_is_counted_by_row() {
        let rows: HeaderProjection = vec![
            ("WWW-Authenticate".to_string(), "Digest realm=\"a\", nonce=\"n1\"".to_string()),
            ("www-authenticate".to_string(), "Digest realm=\"b\", nonce=\"n2\"".to_string()),
            ("Date".to_string(), "Sat, 13 Nov 2010 23:29:00 GMT".to_string()),
        ]
        .into();
        assert_eq!(rows.item("WWW-Authenticate", 1), Some("Digest realm=\"b\", nonce=\"n2\""));
        assert_eq!(rows.item("WWW-Authenticate", 2), None);
        assert_eq!(rows.item("Date", 0), Some("Sat, 13 Nov 2010 23:29:00 GMT"));
        assert_eq!(rows.item("Date", 1), None);
    }
}
