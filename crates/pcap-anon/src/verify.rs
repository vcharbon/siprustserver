//! The leak check after a rewrite (`--verify`): every number-bearing
//! user-part of the RAW document is searched in the anonymised one, and the
//! count of distinct survivors is the verdict — reported as a count, never as
//! a number.
//!
//! A needle is the leading digit run, at least `MIN_KEY_WIDTH` wide, of every
//! harvested URI user-part, of every string claim of every PASSporT payload
//! and of every digit run of every User-to-User payload that reads, the raw
//! datagram harvested as the anonymiser harvests it: as written, and again
//! with its base64 text that is beyond accident decoded ([`crate::encoded`]).
//! A survivor is a needle found as a WHOLE digit run — not inside a longer
//! one, since a fake's hash digits spell anything — left in place: the
//! rewrite keeps every datagram's width, so where the raw datagram holds
//! other digits at that offset the run is a fake or a composed rewrite that
//! spells another number by chance (a datagram whose width changed is
//! searched whole). It is searched in the anonymised bytes
//! with each PASSporT segment and hex UUI replaced by its decoded payload,
//! and again in every base64 text of the datagram, decoded where it stands,
//! outside the protocol counters the rewrite keeps (the CSeq and
//! Content-Length values, an SDP rtpmap rate) and an SDP `o=` sess-version, a
//! counter that names no one. In an `o=` sess-id, where the rewrite keeps a
//! short key spelt inside a longer run, a needle that is only ever a short
//! code's composed form (CC + short code) is no survivor. A hex UUI
//! too short to decode into a number's width stays as written.
//!
//! The display-name check reads the anonymised document alone: a display
//! name that still carries a non-ASCII byte or a letter other than the masks
//! is an unmasked name ([`unmasked_displays`]), counted, never shown. The
//! names pass has its own check ([`names::name_survivors`]), and an
//! independent one judges the anonymised bytes alone by the fakes the run
//! wrote ([`leaks::leaks`]).
//!
//! A check that searches for what the raw document held reads every base64
//! text decoded: random bytes spell no needle. A check that judges the
//! anonymised document by its shapes reads decoded the text that is beyond
//! accident ([`crate::encoded::Text::sure`]) and the rest as written: random
//! bytes that decode to text by accident are shaped like anything.
//!
//! No check reads protocol text (a reason phrase, a Reason's or a Warning's
//! text, a control-protocol attribute value): the rewrite never anonymises
//! it, so every check blanks it by position first
//! ([`without_protocol_text`]).

use std::collections::{HashMap, HashSet};

pub mod dialogs;
pub mod leaks;
pub mod names;
pub mod verdict;

mod haystack;
use haystack::{haystack, left_in_place};

use crate::encoded::{self, Context};
use crate::fake::address::Scope;
use crate::identities::PHONE_WIDTH;
use crate::identities::{self, BodyTypes};
use crate::model::Flows;
use crate::passport;
use crate::plan::{NumberClass, Plan, MIN_KEY_WIDTH};
use crate::rewrite::{origin_field, protocol_counter_run_at};

/// `raw` with every protocol-text span ([`crate::rewrite::protocol_spans`])
/// blanked to spaces, its width kept, its control documents those of the
/// anonymiser and of `types`.
pub(crate) fn without_protocol_text(raw: &[u8], types: &BodyTypes) -> Vec<u8> {
    let mut out = raw.to_vec();
    for span in crate::rewrite::protocol_spans::protocol_spans(raw, types) {
        out[span].fill(b' ');
    }
    out
}

/// The distinct number-bearing user-parts of `raw` that survive in
/// `anonymised`, both read under `scope`'s body types.
pub fn survivors(raw: &Flows, anonymised: &Flows, plan: &Plan, scope: &Scope) -> usize {
    let found: HashSet<String> =
        survivor_sites(raw, anonymised, plan, scope).into_iter().map(|s| s.needle).collect();
    found.len()
}

/// One surviving number: the leg and message of the anonymised document it
/// stands in, its byte offset in that datagram (an encoded payload's, for a
/// number decoded from one), and the needle it spells —
/// for a check that locates it, never for a report that prints it.
pub struct SurvivorSite {
    pub leg: usize,
    pub msg: usize,
    pub at: usize,
    pub needle: String,
}

/// Every occurrence of a surviving number ([`survivors`]).
pub fn survivor_sites(
    raw: &Flows,
    anonymised: &Flows,
    plan: &Plan,
    scope: &Scope,
) -> Vec<SurvivorSite> {
    let types = scope.bodies();
    let needles = needles(raw, plan, types);
    let cx = Context::of(anonymised, types);
    let mut found = Vec::new();
    for (li, leg) in anonymised.legs.iter().enumerate() {
        for (mi, msg) in leg.msgs.iter().enumerate() {
            let written = crate::model::wire(msg);
            let before = raw.legs.get(li).and_then(|l| l.msgs.get(mi)).map(crate::model::wire);
            let cseq = msg.summary.cseq();
            let keep = (cseq.seq.to_string().into_bytes(), cseq.method.as_bytes().to_vec());
            let site = |(at, needle)| SurvivorSite { leg: li, msg: mi, at, needle };
            let left = left_in(&written, before.as_deref(), &keep, &needles, types);
            found.extend(left.into_iter().map(site));
            let Some(reading) = encoded::read(&written, &cx) else { continue };
            let before = before.and_then(|b| reading.alike(&b));
            let left = left_in(&reading.bytes, before.as_deref(), &keep, &needles, types);
            let decoded = left.into_iter().filter_map(|(at, n)| Some((reading.run_at(at)?, n)));
            found.extend(decoded.map(site));
        }
    }
    found
}

/// Every needle left in place in one reading of an anonymised datagram —
/// `before` the same reading of the datagram before the rewrite, `keep` the
/// CSeq value it states — with its offset in the reading. A phone-width
/// needle left in an `o=` session field counts, a counter or not.
fn left_in(
    reading: &[u8],
    before: Option<&[u8]>,
    keep: &(Vec<u8>, Vec<u8>),
    needles: &HashMap<String, bool>,
    types: &BodyTypes,
) -> Vec<(usize, String)> {
    let blanked = without_protocol_text(reading, types);
    let haystack = haystack(&blanked, types);
    let hay = &haystack.bytes;
    let mut found = Vec::new();
    let mut i = 0;
    while i < hay.len() {
        if !hay[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let (end, exempt) = protocol_counter_run_at(hay, i, Some((&keep.0, &keep.1)));
        let run = std::str::from_utf8(&hay[i..end]).expect("digits are ASCII");
        let phone_in_origin = run.len() >= PHONE_WIDTH
            && needles.get(run) == Some(&true)
            && origin_field(hay, i, end).is_some();
        let counted = (!exempt || phone_in_origin) && run.len() >= MIN_KEY_WIDTH;
        if let Some((needle, _)) = needles.get_key_value(run).filter(|_| counted) {
            if before.is_none_or(|b| left_in_place(&haystack, i, end, &blanked, b)) {
                found.push((haystack.datagram_offset(i), needle.clone()));
            }
        }
        i = end;
    }
    found
}

/// The distinct display names of `anonymised` that still spell something: a
/// non-ASCII byte, or an ASCII letter other than the masks `x`/`X`. A frozen
/// name (`anonymous`, a `urn:`) is left as found by design and not counted.
/// The document is read under `scope`'s body types, each datagram as written
/// and with its base64 text that is beyond accident decoded.
pub fn unmasked_displays(anonymised: &Flows, scope: &Scope) -> usize {
    let cx = Context::of(anonymised, scope.bodies());
    let mut found: HashSet<Vec<u8>> = HashSet::new();
    for msg in anonymised.legs.iter().flat_map(|leg| &leg.msgs) {
        let written = crate::model::wire(msg);
        let decoded = encoded::read_sure(&written, &cx).map(|reading| reading.bytes);
        let displays = [Some(&written), decoded.as_ref()]
            .into_iter()
            .flatten()
            .flat_map(|datagram| identities::harvest(datagram, scope.bodies()).displays);
        for dn in displays {
            let frozen = crate::anonymize::is_frozen_display(&dn);
            let spells = dn
                .iter()
                .any(|&b| !b.is_ascii() || (b.is_ascii_alphabetic() && b != b'x' && b != b'X'));
            if spells && !frozen {
                found.insert(dn);
            }
        }
    }
    found.len()
}

/// The leading digit runs to search for: every harvested user-part,
/// PASSporT claim and User-to-User digit run of `raw` — each datagram read
/// as written and with its base64 text beyond accident decoded — that bears
/// a number and is not a fake already, each with whether it is a phone
/// number (a CC-scoped national number).
fn needles(raw: &Flows, plan: &Plan, types: &BodyTypes) -> HashMap<String, bool> {
    let cx = Context::of(raw, types);
    let mut needles: HashMap<String, bool> = HashMap::new();
    for msg in raw.legs.iter().flat_map(|leg| &leg.msgs) {
        let written = crate::model::wire(msg);
        let decoded = encoded::read_sure(&written, &cx).map(|reading| reading.bytes);
        for (datagram, as_written) in [(Some(&written), true), (decoded.as_ref(), false)] {
            let Some(datagram) = datagram else { continue };
            let h = identities::harvest(datagram, types);
            let claims = h.passports.iter().flat_map(|segment| passport::strings(segment));
            let payloads = h.uuis.iter().filter(|_| as_written);
            let runs = payloads.flat_map(|u| u.digit_runs().unwrap_or_default());
            let inner: Vec<String> = claims.chain(runs).collect();
            for user in h.numbers.iter().chain(&inner).chain(&h.digit_runs) {
                let Some(class) = plan.classify_any(user) else { continue };
                let digits = user.strip_prefix('+').unwrap_or(user);
                let width = digits.bytes().take_while(|b| b.is_ascii_digit()).count();
                // A key of zeros only names no one (the anonymiser keeps it).
                if class.key().bytes().all(|b| b == b'0') {
                    continue;
                }
                let phone = matches!(class, NumberClass::Nsn { .. });
                *needles.entry(digits[..width].to_string()).or_insert(false) |= phone;
            }
        }
    }
    needles
}

#[cfg(test)]
mod tests;
