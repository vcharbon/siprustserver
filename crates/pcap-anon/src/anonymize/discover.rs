//! Discovery: one walk over a raw document collecting what the passes map —
//! every number key under the plan's classification, the display names, the
//! payloads, the trunk ids, the separated and composed spellings, the names
//! and the identifier words.

use std::collections::{HashMap, HashSet};

use super::{is_frozen_display, names, MappingEntry};
use crate::encoded::{self, Context};
use crate::fake::{fake_keeping, Seed};
use crate::identities::{self, BodyTypes, UuiEncoding};
use crate::model::Flows;
use crate::passport;
use crate::plan::{NumberClass, Plan};
use crate::rewrite::DigitSubs;

/// Walk every datagram in capture order and collect the mapping inputs: number
/// keys (via the plan's total classification, the string claims of every
/// PASSporT payload and the digit runs of every User-to-User payload that
/// reads included; keys already in the fake space are skipped — idempotence), the
/// raw non-frozen display names (`anonymous`, `urn:` names dropped), the
/// payloads and the trunk ids dialed over. Display masking and the payload
/// rewrites are deferred to [`anonymize`] because they need the resolved
/// mapping.
///
/// A datagram holding base64 text that is beyond accident
/// ([`crate::encoded::Text::sure`]) is read twice, as written and with that
/// text decoded where it stands: the decoded reading adds what the same text
/// in clear would add, and nothing else. A shorter text adds no key — random
/// bytes spell one by accident — and is rewritten by the keys the rest of
/// the capture gives. A User-to-User payload is read where it is written,
/// under its encoding.
///
/// A key's form is decided once every occurrence is seen, so it is the same
/// in any discovery order: a key that occurs under a recognised form anywhere
/// (an NSN, a short code, a private number) maps wholly into the reserved
/// space; only a key seen as unrecognised everywhere keeps its head.
pub(super) fn discover<'a>(
    flows: &Flows,
    plan: &Plan,
    seed: &Seed,
    types: &'a BodyTypes,
) -> Discovered<'a> {
    let cx = Context::of(flows, types);
    // Per key, in first-occurrence order: the class its fake is built from
    // (a recognised one wins over an unrecognised reading).
    let mut classes: Vec<NumberClass> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut raw_displays: Vec<Vec<u8>> = Vec::new();
    let mut seen_displays: HashSet<Vec<u8>> = HashSet::new();
    let mut passports: Vec<String> = Vec::new();
    let mut seen_passports: HashSet<String> = HashSet::new();
    let mut trunks: Vec<String> = Vec::new();
    let mut seen_trunks: HashSet<String> = HashSet::new();
    let mut separated: Vec<String> = Vec::new();
    let mut composed: Vec<(String, String)> = Vec::new();
    let mut seen_composed: HashSet<String> = HashSet::new();
    let mut uuis: Vec<String> = Vec::new();
    let mut seen_uuis: HashSet<String> = HashSet::new();
    let mut skipped_uuis: HashSet<String> = HashSet::new();
    let mut names = names::Found::default();
    let mut identifier_words = HashSet::new();
    let mut short_identifier_words = HashSet::new();
    let mut zero_numbers = Vec::new();
    let datagrams = flows.legs.iter().flat_map(|leg| &leg.msgs).map(crate::model::wire);
    for written in datagrams {
        let decoded = encoded::read_sure(&written, &cx).map(|reading| reading.bytes);
        for (raw, as_written) in [(Some(&written), true), (decoded.as_ref(), false)] {
            let Some(raw) = raw else { continue };
            let words = crate::rewrite::identifiers::words(raw);
            identifier_words.extend(words.into_iter().map(|r| raw[r].to_vec()));
            let short = crate::rewrite::identifiers::short_words(raw);
            short_identifier_words.extend(short.into_iter().map(|r| raw[r].to_vec()));
            let h = identities::harvest(raw, types);
            names.absorb(&h.names);
            separated.extend(h.separated.iter().cloned());
            let claims = h.passports.iter().flat_map(|segment| passport::strings(segment));
            for segment in &h.passports {
                if seen_passports.insert(segment.clone()) {
                    passports.push(segment.clone());
                }
            }
            let mut runs: Vec<String> = Vec::new();
            for u in h.uuis.iter().filter(|_| as_written) {
                match u.digit_runs() {
                    Some(found) => {
                        if u.encoding == UuiEncoding::Hex && seen_uuis.insert(u.payload.clone()) {
                            uuis.push(u.payload.clone());
                        }
                        runs.extend(found);
                    }
                    None => {
                        skipped_uuis.insert(u.payload.clone());
                    }
                }
            }
            for user in h
                .numbers
                .iter()
                .chain(&claims.collect::<Vec<_>>())
                .chain(&runs)
                .chain(&h.digit_runs)
            {
                if user.eq_ignore_ascii_case("anonymous") {
                    continue;
                }
                if let Some(trunk) = plan.trunk_id(user) {
                    if seen_trunks.insert(trunk.to_string()) {
                        trunks.push(trunk.to_string());
                    }
                }
                let Some(class) = plan.classify_any(user) else { continue };
                // Zeros only name no one: a key of them would split every run of zeros.
                if class.key().bytes().all(|b| b == b'0') {
                    zero_numbers.push(leading_digit_run(user).to_string());
                    continue;
                }
                let run = leading_digit_run(user);
                if run != class.key() && seen_composed.insert(run.to_string()) {
                    composed.push((run.to_string(), class.key().to_string()));
                }
                match index.get(class.key()) {
                    Some(&at) => {
                        if matches!(classes[at], NumberClass::Unrecognised { .. }) {
                            classes[at] = class;
                        }
                    }
                    None => {
                        index.insert(class.key().to_string(), classes.len());
                        classes.push(class);
                    }
                }
            }
            for dn in &h.displays {
                if is_frozen_display(dn) {
                    continue;
                }
                if seen_displays.insert(dn.clone()) {
                    raw_displays.push(dn.clone());
                }
            }
        }
    }
    // The recognised keys map first; an unrecognised run that embeds one is
    // a composed form of it and takes the display-run form behind its head.
    // (A key's own digits carry no `+`, so anchors play no part here.)
    let recognised = DigitSubs::new(
        classes
            .iter()
            .filter(|class| !matches!(class, NumberClass::Unrecognised { .. }))
            .map(|class| {
                (
                    class.key().as_bytes().to_vec(),
                    fake_keeping(plan, seed, class.key(), 0).into_bytes(),
                )
            })
            .collect(),
    );
    let entries = classes
        .iter()
        .map(|class| {
            let key = class.key();
            let kept = plan.kept_prefix(class);
            let fake = match recognised.earliest(key.as_bytes()) {
                Some(at) if matches!(class, NumberClass::Unrecognised { .. }) => {
                    let kept = kept.min(at);
                    let tail = recognised.apply_zeroing(&key.as_bytes()[kept..]);
                    format!("{}{}", &key[..kept], String::from_utf8(tail).expect("digits"))
                }
                _ => fake_keeping(plan, seed, key, kept),
            };
            MappingEntry { class: class.label(), original: key.to_string(), fake }
        })
        .collect();
    Discovered {
        entries,
        raw_displays,
        passports,
        uuis,
        uui_skipped: skipped_uuis.len(),
        trunks,
        separated,
        composed,
        names,
        identifier_words,
        short_identifier_words,
        zero_numbers,
        cx,
    }
}

/// What discovery collected: the mapping, the raw display names to mask,
/// every distinct PASSporT payload segment, every distinct hex User-to-User
/// payload (and the count of the others), every distinct trunk id, every
/// number spelt with visual separators and every distinct composed run, all
/// in first-occurrence order.
pub(super) struct Discovered<'a> {
    pub(super) entries: Vec<MappingEntry>,
    pub(super) raw_displays: Vec<Vec<u8>>,
    pub(super) passports: Vec<String>,
    pub(super) uuis: Vec<String>,
    pub(super) uui_skipped: usize,
    pub(super) trunks: Vec<String>,
    /// Every number-bearing user-part written with visual separators.
    pub(super) separated: Vec<String>,
    /// Every leading digit run that composes a key with other digits (a CC,
    /// a national or trunk prefix), with that key.
    pub(super) composed: Vec<(String, String)>,
    /// The names outside the numbers.
    pub(super) names: names::Found,
    /// Every word of a dialog or transaction identifier.
    pub(super) identifier_words: HashSet<Vec<u8>>,
    /// Every word of an identifier too short to map, which stays as found.
    pub(super) short_identifier_words: HashSet<Vec<u8>>,
    /// The leading digit run of every number whose key is zeros only, kept.
    pub(super) zero_numbers: Vec<String>,
    /// What the readers of base64 text know of the document.
    pub(super) cx: Context<'a>,
}

/// The leading digit run of a user-part, behind an optional `+`.
fn leading_digit_run(user: &str) -> &str {
    let digits = user.strip_prefix('+').unwrap_or(user);
    &digits[..digits.bytes().take_while(u8::is_ascii_digit).count()]
}
