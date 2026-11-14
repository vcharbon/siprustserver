//! The bytes the survivor check scans in one datagram, each with where it
//! comes from, and whether a run of them stands where the raw datagram holds
//! the same bytes.

use crate::identities::{self, BodyTypes, UuiEncoding};
use crate::passport;
use crate::plan::MIN_KEY_WIDTH;
use crate::uui;

/// Where a byte of a haystack comes from.
#[derive(Clone, Copy)]
enum Origin {
    /// The datagram's bytes from this offset, `width` of them (an escaped
    /// digit is three).
    Datagram { at: usize, width: usize },
    /// Byte `index` of the decoded payload of the encoded segment `segment`.
    Decoded { segment: usize, index: usize },
    /// A separator the haystack adds around a decoded payload.
    Separator,
}

/// One encoded segment of a datagram: where its text stands and how it reads.
struct Segment {
    at: usize,
    len: usize,
    passport: bool,
}

/// The bytes to scan for one datagram, each with its [`Origin`]: the datagram
/// with every PASSporT payload segment and every hex User-to-User payload wide
/// enough to carry a number replaced by its decoded bytes wherever it occurs
/// (between `.` separators, so runs never glue), the encoded text itself —
/// whose digit runs spell anything — left out, and every escaped digit
/// unescaped.
pub(super) struct Haystack {
    pub(super) bytes: Vec<u8>,
    origins: Vec<Origin>,
    segments: Vec<Segment>,
}

impl Haystack {
    /// Where byte `i` stands in the datagram: its own offset, or the offset
    /// of the encoded segment it was decoded from.
    pub(super) fn datagram_offset(&self, i: usize) -> usize {
        match self.origins.get(i) {
            Some(Origin::Datagram { at, .. }) => *at,
            Some(Origin::Decoded { segment, .. }) => self.segments[*segment].at,
            _ => self.origins[..i]
                .iter()
                .rev()
                .find_map(|o| match o {
                    Origin::Datagram { at, .. } => Some(*at),
                    _ => None,
                })
                .unwrap_or(0),
        }
    }
}

pub(super) fn haystack(raw: &[u8], types: &BodyTypes) -> Haystack {
    let h = identities::harvest(raw, types);
    let mut encoded: Vec<(Vec<u8>, Vec<u8>, bool)> = h
        .passports
        .iter()
        .filter_map(|s| Some((s.clone().into_bytes(), passport::decode(s)?.0, true)))
        .chain(h.uuis.iter().filter(|u| u.encoding == UuiEncoding::Hex).filter_map(|u| {
            let decoded = uui::decode(&u.payload)?;
            (decoded.len() >= MIN_KEY_WIDTH)
                .then(|| (u.payload.clone().into_bytes(), decoded, false))
        }))
        .collect();
    encoded.sort();
    encoded.dedup();
    let mut hay = Haystack { bytes: Vec::new(), origins: Vec::new(), segments: Vec::new() };
    let mut last = 0;
    loop {
        let next = encoded
            .iter()
            .filter_map(|(text, decoded, passport)| {
                Some((find(&raw[last..], text)? + last, text, decoded, *passport))
            })
            .min_by_key(|(at, text, _, _)| (*at, std::cmp::Reverse(text.len())));
        let Some((at, text, decoded, passport)) = next else { break };
        unescape_digits(raw, last, at, &mut hay);
        let segment = hay.segments.len();
        hay.segments.push(Segment { at, len: text.len(), passport });
        hay.bytes.push(b'.');
        hay.origins.push(Origin::Separator);
        for (index, &b) in decoded.iter().enumerate() {
            hay.bytes.push(b);
            hay.origins.push(Origin::Decoded { segment, index });
        }
        hay.bytes.push(b'.');
        hay.origins.push(Origin::Separator);
        last = at + text.len();
    }
    unescape_digits(raw, last, raw.len(), &mut hay);
    hay
}

/// `raw[from..to]` onto the haystack, every escaped digit (`%30`…`%39`, RFC
/// 3261 §19.1.2) written as the digit, so an escaped number reads as the
/// digit run it spells.
fn unescape_digits(raw: &[u8], from: usize, to: usize, hay: &mut Haystack) {
    let mut i = from;
    while i < to {
        if let [b'%', b'3', d, ..] = raw[i..to] {
            if d.is_ascii_digit() {
                hay.bytes.push(d);
                hay.origins.push(Origin::Datagram { at: i, width: 3 });
                i += 3;
                continue;
            }
        }
        hay.bytes.push(raw[i]);
        hay.origins.push(Origin::Datagram { at: i, width: 1 });
        i += 1;
    }
}

/// Whether the run `hay.bytes[start..end]` of the anonymised datagram `out`
/// stands where the raw datagram `raw` holds the same bytes: compared in
/// the datagrams where the run is datagram bytes, in the decoded payloads of
/// the segments at the same place where it is decoded bytes. A datagram or a
/// segment whose width changed is searched whole: every run stands in place.
pub(super) fn left_in_place(
    hay: &Haystack,
    start: usize,
    end: usize,
    out: &[u8],
    raw: &[u8],
) -> bool {
    if raw.len() != out.len() {
        return true;
    }
    match (hay.origins[start], hay.origins[end - 1]) {
        (Origin::Datagram { at, .. }, Origin::Datagram { at: last, width }) => {
            raw[at..last + width] == out[at..last + width]
        }
        (Origin::Decoded { segment, index }, Origin::Decoded { index: last, .. }) => {
            let s = &hay.segments[segment];
            let text = String::from_utf8_lossy(&raw[s.at..s.at + s.len]);
            let decoded =
                if s.passport { passport::decode(&text).map(|d| d.0) } else { uui::decode(&text) };
            decoded.is_none_or(|d| d.get(index..=last) == Some(&hay.bytes[start..end]))
        }
        _ => true,
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
