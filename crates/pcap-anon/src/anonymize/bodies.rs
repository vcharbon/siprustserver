//! Body parts the anonymiser does not understand
//! ([`identities::bodies::understood`], under the network's body types; a
//! control type's part that is no XML document among them) are
//! redacted whole: every byte of the
//! part's content but its line breaks becomes `x`, and so does the value of
//! each of its entity headers but `Content-Type` and
//! `Content-Transfer-Encoding`, so the datagram keeps its length and its MIME
//! framing and carries nothing of the part. A line keeps its length: how long
//! each line of a redacted part was stays readable. A multipart body's bytes
//! outside its parts and its delimiter lines — preamble, epilogue, a body its
//! boundary frames nothing of — are redacted the same way.

use crate::identities::{self, BodyTypes};

/// The entity headers a redacted part keeps: its framing.
const FRAMING: &[&str] = &["content-type", "content-transfer-encoding"];

/// `raw` with every part of a media type the anonymiser does not understand
/// redacted, and every stretch of a multipart body outside its parts and its
/// delimiter lines ([`identities::outside_parts`]); how many parts and
/// stretches holding more than line breaks were.
pub(super) fn redact_unknown(raw: &[u8], types: &BodyTypes) -> (Vec<u8>, usize) {
    let mut out = raw.to_vec();
    let mut redacted = 0;
    for part in identities::body_parts(raw) {
        if identities::bodies::understood(&part, raw, types) {
            continue;
        }
        let headers = part
            .headers
            .iter()
            .filter(|(n, _)| !FRAMING.contains(&n.to_ascii_lowercase().as_str()));
        let spans =
            std::iter::once(part.start..part.end).chain(headers.map(|(_, span)| span.clone()));
        for span in spans {
            redact(&mut out[span]);
        }
        redacted += 1;
    }
    for span in identities::outside_parts(raw) {
        if out[span.clone()].iter().any(|b| !matches!(*b, b'\r' | b'\n')) {
            redact(&mut out[span]);
            redacted += 1;
        }
    }
    (out, redacted)
}

/// Every byte of `bytes` but a line break becomes `x`.
fn redact(bytes: &mut [u8]) {
    for b in bytes {
        if !matches!(*b, b'\r' | b'\n') {
            *b = b'x';
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identities::body_types::{BodyReading, BodyType};

    /// A MESSAGE whose multipart body carries SDP, a PIDF document and an
    /// order of the vendor type `application/vnd.example.order`.
    fn message() -> String {
        let body = "--b\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n--b\r\n\
                    Content-Type: application/pidf+xml\r\nContent-Location: <http://loc.b.zz/l>\r\n\r\n\
                    <presence entity=\"pres:a@b.zz\"/>\r\n--b\r\n\
                    Content-Type: application/vnd.example.order\r\n\r\norder 5550100\r\n--b--\r\n";
        format!(
            "MESSAGE sip:a@b SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1;branch=z9hG4bK1\r\nMax-Forwards: 70\r\n\
             From: <sip:a@b>;tag=1\r\nTo: <sip:a@b>\r\nCall-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: multipart/mixed;boundary=b\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn a_part_not_understood_is_redacted_whole_and_an_understood_one_stays() {
        let raw = message();
        let (out, redacted) = redact_unknown(raw.as_bytes(), &BodyTypes::default());
        assert_eq!(redacted, 2);
        assert_eq!(out.len(), raw.len());
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("v=0")
                && !text.contains("presence")
                && !text.contains("loc.b")
                && !text.contains("order 5550100"),
            "{text}"
        );
        assert!(text.contains("Content-Type: application/pidf+xml"), "{text}");
        assert!(identities::parses(text.as_bytes(), &crate::testflow::reading()));
    }

    /// A body type the network states is read, not redacted.
    #[test]
    fn a_part_of_a_stated_body_type_stays() {
        let types = BodyTypes::new(vec![BodyType::of(
            "application/vnd.example.order",
            BodyReading::DigitRuns,
        )])
        .expect("a bare media type");
        let (out, redacted) = redact_unknown(message().as_bytes(), &types);
        assert_eq!(redacted, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("order 5550100") && !text.contains("presence"), "{text}");
    }
}
