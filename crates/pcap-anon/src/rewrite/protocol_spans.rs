//! Where protocol text stands in a datagram: each status line's reason
//! phrase (a response's and a sipfrag's), each quoted text of a Reason or a
//! Warning header, and each control-protocol attribute value of a control
//! document part ([`is_control`]) that reads as an XML document
//! ([`control_document`]). Protocol text is never anonymised and
//! never read by the leak checks, whatever it holds: it is kept verbatim
//! because of where it sits. A warn-agent is a field, not text, and maps.

use std::ops::Range;

use sip_message::sniff::header_row_spans;

use crate::identities::bodies::{control_document, is_control};
use crate::identities::BodyTypes;

/// Every protocol-text span of `raw`, in order, none overlapping, its control
/// documents those of the anonymiser and of `types`.
pub fn protocol_spans(raw: &[u8], types: &BodyTypes) -> Vec<Range<usize>> {
    let mut out = super::fields::reason_phrases(raw);
    let rows = ["Reason", "Warning"].into_iter().flat_map(|name| header_row_spans(raw, name));
    for row in rows {
        let Ok(text) = std::str::from_utf8(&raw[row.clone()]) else { continue };
        let Some((name, value)) = text.split_once(':') else { continue };
        let base = row.start + name.len() + 1;
        for (_, quoted) in crate::identities::quoted_all(value) {
            let offset = quoted.as_ptr() as usize - value.as_ptr() as usize;
            out.push(base + offset..base + offset + quoted.len());
        }
    }
    let parts = crate::identities::body_parts(raw);
    for part in parts.into_iter().filter(|p| is_control(&p.media_type, types)) {
        let content = &raw[part.start..part.end];
        let Some(doc) = std::str::from_utf8(content).ok().filter(|_| control_document(content))
        else {
            continue;
        };
        let data = crate::xml::data(doc).into_iter().filter(crate::xml::Datum::is_protocol);
        out.extend(data.map(|d| part.start + d.at..part.start + d.at + d.value.len()));
    }
    out.sort_by_key(|r| r.start);
    out.dedup_by(|later, earlier| later.start < earlier.end);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phrase_the_quoted_texts_and_the_control_attributes_are_protocol_text() {
        let body = "<MediaServerControl version=\"1.0\"><response request=\"play\" \
                    reason=\"fakeloop\"/><who>Fakey</who></MediaServerControl>";
        let raw = format!(
            "SIP/2.0 486 Fakey Busy\r\nVia: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKf1\r\n\
             From: <sip:fakey@fake-pbx.notreal-carrier.de>;tag=f1\r\n\
             To: <sip:testo@fake-pbx.notreal-carrier.de>;tag=f2\r\n\
             Call-ID: f1@fake-pbx.notreal-carrier.de\r\nCSeq: 1 INFO\r\n\
             Reason: Q.850;cause=17;text=\"Fakey busy\"\r\n\
             Warning: 399 fake-sbc.notreal-carrier.de \"Fakey off\"\r\n\
             Accept: application/isup\r\nContent-Type: application/mediaservercontrol+xml\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let spans: Vec<&str> = protocol_spans(raw.as_bytes(), &BodyTypes::default())
            .into_iter()
            .map(|r| &raw[r])
            .collect();
        assert_eq!(spans, [" Fakey Busy", "Fakey busy", "Fakey off", "1.0", "play", "fakeloop"]);
    }

    /// A control document of a type the network states has its protocol
    /// attributes as protocol text; the same document of a type no one
    /// states has none.
    #[test]
    fn a_stated_control_type_has_its_attributes_as_protocol_text() {
        let body = "<ctl version=\"2.0\"><play reason=\"fakeloop\"/><who>Fakey</who></ctl>";
        let raw = format!(
            "INFO sip:fakey@192.0.2.10 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKf1\r\n\
             From: <sip:fakey@192.0.2.10>;tag=f1\r\nTo: <sip:testo@192.0.2.20>;tag=f2\r\n\
             Call-ID: f1@192.0.2.10\r\nCSeq: 2 INFO\r\nMax-Forwards: 70\r\n\
             Content-Type: application/vnd.example.ctl+xml\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let stated = BodyTypes::new(vec![crate::identities::body_types::BodyType::of(
            "application/vnd.example.ctl+xml",
            crate::identities::body_types::BodyReading::Control,
        )])
        .expect("a bare media type");
        let spans = |types: &BodyTypes| -> Vec<String> {
            protocol_spans(raw.as_bytes(), types).into_iter().map(|r| raw[r].to_string()).collect()
        };
        assert_eq!(spans(&stated), ["2.0", "fakeloop"]);
        assert!(spans(&BodyTypes::default()).is_empty());
    }
}
