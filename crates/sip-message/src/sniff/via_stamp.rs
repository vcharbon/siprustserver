//! The stamps a relay leaves on the Via of the hop it received a request
//! from: where it located that hop's `received` (RFC 3261 §18.2.1) and filled
//! `rport` (RFC 3581 §4) parameters.

use std::ops::Range;

/// The spans of the `;received=<value>` and `;rport=<value>` parameters on
/// the SECOND Via entry of `raw` (comma folds split, §7.3.1), in wire order —
/// the Via of the hop the top-Via sender received the request from, which it
/// stamps with the copy's source address and port (RFC 3261 §18.2.1, RFC 3581
/// §4). Empty where that Via carries neither, or the head has fewer than two
/// Via entries.
pub fn relay_stamp_spans(raw: &[u8]) -> Vec<Range<usize>> {
    let mut entry = 0usize;
    let mut pos = 0;
    while let Some(rest) = raw.get(pos..).filter(|rest| !rest.is_empty()) {
        let line_end = rest.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| pos + p);
        let Some(line) = raw.get(pos..line_end) else { break };
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return Vec::new(); // end of headers
        }
        let mut halves = line.splitn(2, |&b| b == b':');
        if let (Some(name), Some(value)) = (halves.next(), halves.next()) {
            let colon = name.len();
            let name = String::from_utf8_lossy(name);
            let name = name.trim();
            if name.eq_ignore_ascii_case("via") || name.eq_ignore_ascii_case("v") {
                let mut start = colon + 1;
                for part in value.split(|&b| b == b',') {
                    if entry == 1 {
                        let at = pos + start;
                        let mut spans: Vec<Range<usize>> = [b";received=".as_slice(), b";rport="]
                            .into_iter()
                            .filter_map(|key| param_span(part, key))
                            .map(|r| at + r.start..at + r.end)
                            .collect();
                        spans.sort_by_key(|r| r.start);
                        return spans;
                    }
                    entry += 1;
                    start += part.len() + 1;
                }
            }
        }
        pos = line_end + 1;
    }
    Vec::new()
}

/// The span of the parameter that opens with `key` (case-insensitive) in
/// `via`, through the end of its value.
fn param_span(via: &[u8], key: &[u8]) -> Option<Range<usize>> {
    let lower: Vec<u8> = via.iter().map(u8::to_ascii_lowercase).collect();
    let start = lower.windows(key.len()).position(|w| w == key)?;
    let value = start + key.len();
    let end = via
        .get(value..)?
        .iter()
        .position(|&b| matches!(b, b';' | b',' | b' ' | b'\t'))
        .map_or(via.len(), |p| value + p);
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAYED: &[u8] = b"INVITE sip:bob@192.0.2.20 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.9:5060;branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5092;branch=z9hG4bKw;rport=5092;received=10.0.2.1\r\n\
Call-ID: c\r\n\r\n";

    #[test]
    fn the_second_vias_rport_and_received_are_located() {
        let spans = relay_stamp_spans(RELAYED);
        let stamps: Vec<&[u8]> = spans.into_iter().map(|r| &RELAYED[r]).collect();
        assert_eq!(stamps, [b";rport=5092".as_slice(), b";received=10.0.2.1"]);
    }

    #[test]
    fn a_folded_second_entry_is_located_too() {
        let raw = b"ACK sip:b SIP/2.0\r\nVia: SIP/2.0/UDP a:1;branch=x, SIP/2.0/UDP b:2;received=c;branch=y\r\n\r\n";
        let spans = relay_stamp_spans(raw);
        assert_eq!(spans.len(), 1);
        assert_eq!(&raw[spans[0].clone()], b";received=c");
    }

    #[test]
    fn the_top_vias_stamps_and_an_unstamped_second_are_not() {
        let raw =
            b"ACK sip:b SIP/2.0\r\nVia: SIP/2.0/UDP a:1;received=z\r\nVia: SIP/2.0/UDP b:2\r\n\r\n";
        assert!(relay_stamp_spans(raw).is_empty());
        assert!(relay_stamp_spans(b"ACK sip:b SIP/2.0\r\nVia: SIP/2.0/UDP a:1\r\n\r\n").is_empty());
    }
}
