//! Where base64 text stands in a datagram ([`sites`]), the datagram read
//! with it decoded ([`decoded`]), and the rewrite of a datagram through both
//! readings ([`rewrite`]).
//!
//! A run that decodes to text ([`super::runs`]) is a site unless it stands
//! where it is no text of its own: in a header's name, in a PASSporT (its
//! own pass's), in protocol text, in a body part the anonymiser redacts
//! whole, or where it is or holds a dialog or transaction identifier word
//! (the identifier pass's, wherever a copy stands).

use std::collections::HashSet;
use std::ops::Range;

use sip_message::sniff;

use super::Text;
use crate::identities::bodies::understood;
use crate::identities::BodyTypes;
use crate::model::Flows;
use crate::rewrite::fields::{header_name, line_at};
use crate::rewrite::identifiers::{self, is_identifier, param_name};
use crate::rewrite::protocol_spans::protocol_spans;

/// What the readers of one document know of it beyond a datagram: the words
/// of its dialog and transaction identifiers, as written, and the body types
/// read.
pub struct Context<'a> {
    identifiers: HashSet<Vec<u8>>,
    types: &'a BodyTypes,
}

impl<'a> Context<'a> {
    /// The context of a document whose identifier words are `identifiers`.
    pub fn new(identifiers: HashSet<Vec<u8>>, types: &'a BodyTypes) -> Context<'a> {
        Context { identifiers, types }
    }

    /// The context of `flows`.
    pub fn of(flows: &Flows, types: &'a BodyTypes) -> Context<'a> {
        let mut identifiers = HashSet::new();
        for msg in flows.legs.iter().flat_map(|leg| &leg.msgs) {
            let raw = crate::model::wire(msg);
            identifiers.extend(identifiers::words(&raw).into_iter().map(|r| raw[r].to_vec()));
        }
        Context { identifiers, types }
    }
}

/// One site: where its run stands, the text it decodes to, and whether that
/// text is beyond accident ([`Text::sure`], or a User-to-User payload its
/// `encoding` parameter states base64).
#[derive(Debug, Clone)]
pub struct Site {
    pub at: Range<usize>,
    pub text: Text,
    pub sure: bool,
}

/// Every site of `raw` (module doc), in order.
pub fn sites(raw: &[u8], cx: &Context) -> Vec<Site> {
    let found = super::runs(raw);
    if found.is_empty() {
        return Vec::new();
    }
    let mut closed = protocol_spans(raw, cx.types);
    let head = sniff::head_end(raw).unwrap_or(raw.len());
    if found.iter().any(|(at, _)| at.end > head) {
        let parts = crate::identities::body_parts(raw);
        for part in parts.into_iter().filter(|p| !understood(p, raw, cx.types)) {
            closed.push(part.start..part.end);
            closed.extend(part.headers.into_iter().map(|(_, span)| span));
        }
        closed.extend(crate::identities::outside_parts(raw));
    }
    found
        .into_iter()
        .filter(|(at, _)| !closed.iter().any(|c| c.start < at.end && at.start < c.end))
        .filter(|(at, _)| !own_pass(raw, at, cx))
        .map(|(at, text)| {
            let sure = text.sure() || base64_payload(raw, at.start);
            Site { at, text, sure }
        })
        .collect()
}

/// Whether the run at `at` is another reader's: a header's name, a PASSporT,
/// an identifier or a copy of one.
fn own_pass(raw: &[u8], at: &Range<usize>, cx: &Context) -> bool {
    let line_start = raw[..at.start].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let before = &raw[line_start..at.start];
    let name = header_name(raw, at.start);
    let in_name = name.is_some() && !before.contains(&b':');
    let passport = name.as_deref() == Some("identity") && !before.contains(&b';');
    let word = raw[at.clone()]
        .split(|b| !b.is_ascii_alphanumeric())
        .any(|piece| cx.identifiers.contains(piece));
    in_name || passport || word || is_identifier(raw, at.start, at.end)
}

/// Whether `raw[at]` stands in a User-to-User payload — the header's or an
/// escaped URI header's — whose `encoding` parameter states base64.
fn base64_payload(raw: &[u8], at: usize) -> bool {
    let payload = header_name(raw, at).as_deref() == Some("user-to-user")
        || param_name(raw, at).as_deref() == Some("user-to-user");
    let line = line_at(raw, at).to_ascii_lowercase();
    let states = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
    payload && (states(b"encoding=base64") || states(b"encoding%3dbase64"))
}

/// A datagram with sites decoded: its bytes, where each site's text stands
/// in them (`spans`) and where its run stands in the datagram (`runs`).
pub struct Decoded {
    pub bytes: Vec<u8>,
    pub spans: Vec<Range<usize>>,
    pub runs: Vec<Range<usize>>,
}

impl Decoded {
    /// Where in the datagram the run stands whose text holds byte `at` of
    /// the reading, if one does.
    pub fn run_at(&self, at: usize) -> Option<usize> {
        let site = self.spans.iter().position(|span| span.contains(&at))?;
        Some(self.runs[site].start)
    }

    /// `other`, a datagram as wide as this reading's, read alike: the same
    /// runs decoded. `None` when one of them decodes there to no text of the
    /// same width.
    pub fn alike(&self, other: &[u8]) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(self.bytes.len());
        let mut at = 0;
        for (run, span) in self.runs.iter().zip(&self.spans) {
            let written = std::str::from_utf8(other.get(run.clone())?).ok()?;
            let text = Text::of(written).filter(|text| text.text.len() == span.len())?;
            bytes.extend_from_slice(&other[at..run.start]);
            bytes.extend_from_slice(&text.text);
            at = run.end;
        }
        bytes.extend_from_slice(&other[at..]);
        restate_length(other, &mut bytes);
        Some(bytes)
    }
}

/// `raw` with each of `sites` (in order) replaced by its text. A
/// `Content-Length` that states the body of `raw` states the body of the
/// reading, in as many digits, so the reading parses where `raw` does.
pub fn decoded(raw: &[u8], sites: &[Site]) -> Decoded {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut spans = Vec::with_capacity(sites.len());
    let mut at = 0;
    for site in sites {
        bytes.extend_from_slice(&raw[at..site.at.start]);
        spans.push(bytes.len()..bytes.len() + site.text.text.len());
        bytes.extend_from_slice(&site.text.text);
        at = site.at.end;
    }
    bytes.extend_from_slice(&raw[at..]);
    restate_length(raw, &mut bytes);
    Decoded { bytes, spans, runs: sites.iter().map(|site| site.at.clone()).collect() }
}

/// States the body length of `reading` in its `Content-Length`, zeros ahead,
/// where the one of `raw` states the body `raw` holds and the reading's
/// differs.
fn restate_length(raw: &[u8], reading: &mut [u8]) {
    let lengths = |bytes: &[u8]| {
        sniff::content_length(bytes).zip(sniff::body(bytes).map(|body| body.len() as u64))
    };
    let (Some((stated, held)), Some((_, now))) = (lengths(raw), lengths(reading)) else { return };
    let rows = sniff::header_row_spans(reading, "Content-Length");
    let Some(row) = rows.into_iter().next().filter(|_| stated == held && now != held) else {
        return;
    };
    let Some(colon) = reading[row.clone()].iter().position(|b| *b == b':') else { return };
    let value = row.start + colon + 1;
    let start = value + reading[value..row.end].iter().take_while(|b| !b.is_ascii_digit()).count();
    let width = reading[start..row.end].iter().take_while(|b| b.is_ascii_digit()).count();
    let restated = format!("{now:0width$}");
    if restated.len() == width {
        reading[start..start + width].copy_from_slice(restated.as_bytes());
    }
}

/// `raw` with every site decoded; `None` when it holds none.
pub fn read(raw: &[u8], cx: &Context) -> Option<Decoded> {
    let sites = sites(raw, cx);
    (!sites.is_empty()).then(|| decoded(raw, &sites))
}

/// `raw` with every site whose text is beyond accident decoded; `None` when
/// it holds none.
pub fn read_sure(raw: &[u8], cx: &Context) -> Option<Decoded> {
    let sites: Vec<Site> = sites(raw, cx).into_iter().filter(|site| site.sure).collect();
    (!sites.is_empty()).then(|| decoded(raw, &sites))
}

/// `raw` rewritten by `pass`, a width-keeping rewrite of a datagram, through
/// both readings. The pass over `raw` as written decides everything outside
/// the sites. A site whose text the pass over the decoded reading changes
/// takes that text re-encoded — unless the pass over `raw` rewrote a letter
/// of the run: then the run is an identity as written (a name, a display
/// name, an identifier) and its rewrite as written stands. Digits the pass
/// over `raw` mapped inside a run spell a key by accident and give way.
pub fn rewrite(raw: &[u8], cx: &Context, pass: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = pass(raw);
    let sites = sites(raw, cx);
    if sites.is_empty() || out.len() != raw.len() {
        return out;
    }
    let reading = decoded(raw, &sites);
    let read = pass(&reading.bytes);
    if read.len() != reading.bytes.len() {
        return out;
    }
    for (site, span) in sites.iter().zip(&reading.spans) {
        let text = &read[span.clone()];
        let run = site.at.clone();
        let as_written = raw[run.clone()]
            .iter()
            .zip(&out[run.clone()])
            .all(|(a, b)| a == b || a.is_ascii_digit());
        let encoded = site.text.encode(text);
        if text != site.text.text && as_written && encoded.len() == run.len() {
            out[run].copy_from_slice(encoded.as_bytes());
        }
    }
    out
}
