//! Flow → anonymized flow, as a transformation on the typed model.
//! Discovery walks every
//! datagram's identity surface ([`crate::identities`]) and classifies
//! user-parts through the numbering-plan equivalence config
//! ([`crate::plan`]); each discovered number maps to a same-length fake in
//! the plan's reserved `fake_prefix` digit space. The rewrite is a
//! substring-consistent pass over the ENTIRE datagram bytes (Contact junk
//! user-parts, Call-IDs, bodies included — same-length fakes keep
//! Content-Length valid) and the same substitutions are applied to every
//! mirrored model string, so model and bytes never diverge; numeric summary
//! fields are guarded instead (loud error on collision).
//!
//! Raw documents only: a document the run has anonymised says so
//! (`Flows::rewritten`) and is refused, and nothing tells a fake from a real
//! value by its shape — a regeneration starts from the raw capture under its
//! kept seed, which reproduces the output byte for byte. Every user-part run
//! maps whole, so no shorter key spelt inside it splits it: a digit run
//! display masking writes to itself, a composed run (a key behind a CC, a
//! national or trunk prefix) to itself with the key's fake where the key
//! stands, and a number written with visual separators to its fake with the
//! separators where they stood.
//!
//! Nothing number-bearing is left in clear: every user-part with a leading
//! digit run of at least `MIN_KEY_WIDTH` digits classifies
//! ([`Plan::classify_any`]) — the plan's recognised forms, else the whole run
//! as an unrecognised key that keeps its CC-looking head (or, when it embeds
//! a recognised key, that key's fake behind zeros, like a display run) — and
//! the report counts the unrecognised ones so the plan is improved from a
//! count, never from a leak.
//!
//! A fake is a keyed hash of its key ([`crate::fake`]): `fake_prefix` + the
//! `9` marker + `HMAC-SHA256(seed, key)` reduced to decimal, zero-padded to the
//! KEY's width. The one invariant is consistency — a number maps to one fake
//! everywhere (URI user-parts, Call-IDs, bodies, display names), in any
//! discovery order, and across captures anonymised under the same seed; the
//! mapping is one-way without the seed. The rewrite ([`crate::rewrite`])
//! matches the REAL keys leftmost-longest in one pass, so a nested fake pair
//! rewrites correctly; only per-occurrence counting of the output is ambiguous.
//! Every occurrence of a key's digits is rewritten (composed forms, extensions glued
//! into Call-IDs or SDP) except the message's protocol counters — the CSeq
//! value (its sequence number written before its method, as the CSeq line
//! and a RAck carry it), the Content-Length value, an SDP rtpmap clock rate
//! and a narrow key spelt inside a longer SDP `o=` sess-id or sess-version
//! are framing,
//! not identity, and stay — and the routing a form is dialed under, kept
//! whole: the ingress trunk id and the CC behind a `+` ([`crate::rewrite`]).
//! A trunk id maps to itself, matched leftmost at the head of its dial form,
//! so a shorter key spelling part of it never splits it.
//! The `000…` prefix stays reserved (an NSN never starts with `0`), and the marker
//! sits INSIDE that space, so a fake embedded in a longer composed form shifts
//! only into 0-leading windows the plan rejects — never re-discovered.
//!
//! An `Identity` header's PASSporT ([`crate::passport`]) is part of the
//! surface: its string claims are discovered like user-parts, so the same
//! numbers elsewhere map, and the whole compact form is redacted by the
//! [`names`] pass — a kept signature would confirm a guessed number. A body
//! part of a media type the anonymiser does not understand is redacted
//! whole ([`bodies`]). A hex User-to-User payload
//! ([`crate::uui`]) is handled the same way — its decoded digit runs are dial
//! forms — an `ascii`/`text` one is its own bytes, whose runs are keys the
//! pass rewrites in place, and a payload in any other encoding is counted
//! ([`Report::uui_skipped`]), never guessed at. A base64 run that decodes to
//! text — a base64 User-to-User payload among them — is that text standing
//! in clear at the same place ([`crate::encoded`]): every pass reads the
//! datagram with it decoded too, and what they write into the text is
//! re-encoded.
//!
//! Display names are fully anonymized: basic-ASCII letters go to the fixed
//! points `x`/`X`, every byte of a non-ASCII character to `x` (any script,
//! emoji, a non-UTF-8 byte), and EVERY digit run is neutralized — each discovered number
//! embedded in a run (any composed form) maps to its fake (identical to its URI
//! user-part), and every OTHER digit is zeroed, so the whole run lands in the
//! reserved space (a run with a fake starts `000…`; a pure-orphan run becomes
//! all `0`s). ASCII punctuation and spaces are preserved, and the name keeps
//! its byte length. Same input + same plan + same seed ⇒ same
//! output.
//!
//! The identities that are not numbers — host names, realms, charging ids,
//! product text and addresses — go through the [`names`] pass, applied after
//! this one to the same bytes and strings, under the same seed and a
//! [`Scope`] that says which addresses map.

use std::collections::{HashMap, HashSet};

use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::Flows;
use crate::plan::{Plan, MIN_KEY_WIDTH};
use crate::rewrite::{DigitSubs, DisplaySubs};
use stretches::outside;

mod bodies;
mod collisions;
mod discover;
mod display;
mod document;
pub mod fakes;
mod guard;
mod identifiers;
pub mod names;
mod payload_rewrite;
mod separated;
mod stretches;

#[cfg(test)]
use collisions::group_in_order;
use collisions::{count_collisions, count_merged_call_ids};
use discover::{discover, Discovered};
use display::anonymize_display;
pub use fakes::Fakes;
use guard::guard_numeric_fields;
pub use guard::parse_ok_map;

/// One mapping decision, in discovery order (first wire occurrence).
#[derive(Debug)]
pub struct MappingEntry {
    /// The class label (`NumberClass::label`): `cc <cc>`, `short <cc>`,
    /// `private`, `unrecognised`.
    pub class: String,
    /// Original digit key (NSN, short-code or private digits, the whole run
    /// of an unrecognised number).
    pub original: String,
    /// Same-length fake inside the plan's reserved digit space
    /// ([`crate::fake::fake_keeping`]).
    pub fake: String,
}

/// Two fakes that are not distinct enough for the report to count occurrences
/// 1:1 — a statistic of the run, not a failure (module doc).
#[derive(Debug, PartialEq, Eq)]
pub enum Collision {
    /// Several keys (same width) hash to one fake; the rewrite merges them.
    Shared { fake: String, originals: Vec<String> },
    /// A shorter fake is a substring of a longer one.
    Nested { shorter: String, longer: String },
}

/// What one anonymization run mapped: every entry in discovery order, the
/// collisions among the fakes, and the
/// Call-IDs the rewrite merged.
#[derive(Debug, Default)]
pub struct Report {
    pub entries: Vec<MappingEntry>,
    pub collisions: Vec<Collision>,
    /// Legs whose rewritten Call-ID equals another leg's while the originals
    /// differed (a shared fake merged them).
    pub merged_call_ids: usize,
    /// Distinct User-to-User payloads left as found because their encoding is
    /// neither hex nor ascii/text nor base64 of text (absent, unknown, base64
    /// of bytes that are no text): the count a human decides on.
    pub uui_skipped: usize,
    /// Distinct hex User-to-User payloads that decode to no text, redacted.
    pub uui_redacted: usize,
    /// Every name the names pass mapped ([`names`]), as the number pass left
    /// it: on the wire before this pass, never written out by it.
    pub names: Vec<names::NameEntry>,
    /// Names and addresses sharing their fake with another one.
    pub name_collisions: usize,
    /// Every fake the run wrote, which the leak check judges the output by.
    pub fakes: Fakes,
    /// Body parts of a media type the anonymiser does not understand,
    /// redacted whole.
    pub redacted_parts: usize,
}

impl Report {
    /// Keys that share their fake with at least one other key.
    pub fn shared_keys(&self) -> usize {
        self.collisions
            .iter()
            .map(|c| match c {
                Collision::Shared { originals, .. } => originals.len(),
                Collision::Nested { .. } => 0,
            })
            .sum()
    }

    /// Fake pairs where the shorter is a substring of the longer.
    pub fn nested_fakes(&self) -> usize {
        self.collisions.iter().filter(|c| matches!(c, Collision::Nested { .. })).count()
    }

    /// Keys no form of the plan recognised (mapped whole, CC-looking head
    /// kept): the count the plan gets improved from.
    pub fn unrecognised(&self) -> usize {
        self.entries.iter().filter(|e| e.class == "unrecognised").count()
    }
}

/// Anonymization failure — always loud, never a silent partial rewrite.
#[derive(Debug)]
pub enum AnonymizeError {
    /// A rewrite reached a numeric summary field the model cannot follow
    /// (e.g. NSN digits embedded in CSeq): model and bytes would diverge.
    NumericFieldDiverged { leg: usize, msg: usize, field: &'static str },
    /// The document was rewritten after it was read (`Flows::rewritten`):
    /// only a raw document is anonymised.
    AlreadyRewritten(String),
}

impl std::fmt::Display for AnonymizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnonymizeError::NumericFieldDiverged { leg, msg, field } => write!(
                f,
                "leg {leg} msg {msg}: rewrite reached numeric field {field}; the summary cannot \
                 follow — this capture needs summary re-derivation, refusing partial output"
            ),
            AnonymizeError::AlreadyRewritten(how) => write!(
                f,
                "the document is already {how}: anonymise the raw capture under its kept seed"
            ),
        }
    }
}

impl std::error::Error for AnonymizeError {}

/// Anonymize the raw `flows` in place under `seed` and mark it so; the
/// returned report lists every mapping in discovery order and the collisions
/// among the fakes.
pub fn anonymize(flows: &mut Flows, plan: &Plan, seed: &Seed) -> Result<Report, AnonymizeError> {
    anonymize_in(flows, plan, seed, &Scope::default())
}

/// [`anonymize`], the addresses mapped as `scope` says.
pub fn anonymize_in(
    flows: &mut Flows,
    plan: &Plan,
    seed: &Seed,
    scope: &Scope,
) -> Result<Report, AnonymizeError> {
    if let Some(how) = &flows.rewritten {
        return Err(AnonymizeError::AlreadyRewritten(how.clone()));
    }
    let Discovered {
        entries,
        raw_displays,
        passports,
        uuis,
        uui_skipped,
        trunks,
        separated,
        composed,
        names,
        identifier_words,
        short_identifier_words,
        zero_numbers,
        cx,
    } = discover(flows, plan, seed, scope.bodies());
    let collisions = count_collisions(&entries);

    // The recognised keys are strong, the unrecognised runs weak: a run
    // beside a recognised number never splits it ([`crate::rewrite`]).
    let pairs_of = |unrecognised: bool| -> Vec<(Vec<u8>, Vec<u8>)> {
        entries
            .iter()
            .filter(|e| (e.class == "unrecognised") == unrecognised)
            .map(|e| (e.original.clone().into_bytes(), e.fake.clone().into_bytes()))
            .collect()
    };
    let (strong_pairs, weak_pairs) = (pairs_of(false), pairs_of(true));
    let anchors: Vec<Vec<u8>> = plan.anchors().into_iter().map(String::into_bytes).collect();
    // Whole runs map as one, so no shorter key spelt inside one splits it: a
    // composed user-part maps to itself with its key's fake where the key stands.
    let fake_of: HashMap<&str, &str> =
        entries.iter().map(|e| (e.original.as_str(), e.fake.as_str())).collect();
    let mut taken: HashSet<String> = entries
        .iter()
        .map(|e| e.original.clone())
        .chain(trunks.iter().cloned())
        .chain(plan.anchors())
        .collect();
    let mut whole = |runs: Vec<(String, String)>| -> Vec<(Vec<u8>, Vec<u8>)> {
        runs.into_iter()
            .filter(|(run, _)| taken.insert(run.clone()))
            .map(|(run, to)| (run.into_bytes(), to.into_bytes()))
            .collect()
    };
    let composed_pairs = whole(
        composed
            .iter()
            .filter_map(|(run, key)| {
                let at = run.rfind(key.as_str())?;
                let fake = fake_of.get(key.as_str())?;
                Some((run.clone(), format!("{}{fake}{}", &run[..at], &run[at + key.len()..])))
            })
            .collect(),
    );
    let digit_subs = DigitSubs::anchored(strong_pairs.clone(), weak_pairs.clone(), anchors.clone());
    let separated_pairs = separated::pairs(&separated, &digit_subs);
    let spellings: Vec<Vec<u8>> = separated_pairs.iter().map(|(_, fake)| fake.clone()).collect();
    let payload_subs = DigitSubs::anchored(
        strong_pairs.iter().cloned().chain(composed_pairs.iter().cloned()).collect(),
        weak_pairs.clone(),
        anchors.clone(),
    );

    // Display masking needs the resolved number mapping so a discovered number
    // inside a display name shows the SAME fake as its URI user-part.
    let display_pairs: Vec<(Vec<u8>, Vec<u8>)> = raw_displays
        .iter()
        .filter_map(|dn| {
            let masked = anonymize_display(dn, &digit_subs);
            (masked != *dn).then(|| (dn.clone(), masked))
        })
        .collect();
    let masked_runs: Vec<String> = display_pairs
        .iter()
        .flat_map(|(_, masked)| masked.split(|b| !b.is_ascii_digit()))
        .filter(|run| run.len() >= MIN_KEY_WIDTH)
        .map(|run| String::from_utf8(run.to_vec()).expect("ASCII digits"))
        .collect();
    let display_subs = DisplaySubs::new(display_pairs);

    let payload_rewrite::Rewritten { pairs: payload_pairs, uui_redacted } =
        payload_rewrite::rewritten(&passports, &uuis, &payload_subs);
    let trunk_pairs = trunks.iter().map(|t| (t.clone().into_bytes(), t.clone().into_bytes()));
    let payloads: HashSet<Vec<u8>> =
        passports.iter().chain(&uuis).map(|p| p.clone().into_bytes()).collect();
    let raw_pairs: Vec<(Vec<u8>, Vec<u8>)> = strong_pairs
        .into_iter()
        .chain(payload_pairs)
        .chain(trunk_pairs)
        .chain(separated_pairs)
        .chain(composed_pairs)
        .collect();
    // The digit runs the passes decided: the fakes, the runs display masking
    // wrote, and the zero numbers kept.
    let written: Vec<Vec<u8>> = raw_pairs
        .iter()
        .chain(&weak_pairs)
        .map(|(_, fake)| fake.clone())
        .chain(masked_runs.into_iter().map(String::into_bytes))
        .chain(zero_numbers.into_iter().map(String::into_bytes))
        .collect();
    // A number key spelt as an identifier word takes the number's fake.
    let number_fakes: HashMap<Vec<u8>, Vec<u8>> = raw_pairs
        .iter()
        .chain(&weak_pairs)
        .filter(|(key, fake)| key != fake && key.iter().all(u8::is_ascii_digit))
        .cloned()
        .collect();
    // No name's fake writes a number key as a whole digit run.
    let number_keys: HashSet<Vec<u8>> = number_fakes.keys().cloned().collect();
    // The phone numbers (a CC-scoped national number) and their composed
    // runs: an `o=` session id one of them spells whole maps.
    let mut phones: HashSet<Vec<u8>> = entries
        .iter()
        .filter(|e| e.class.starts_with("cc "))
        .map(|e| e.original.clone().into_bytes())
        .collect();
    let composed_phones: Vec<Vec<u8>> = composed
        .iter()
        .filter(|(_, key)| phones.contains(key.as_bytes()))
        .map(|(run, _)| run.clone().into_bytes())
        .collect();
    phones.extend(composed_phones);
    let raw_subs = DigitSubs::anchored(raw_pairs, weak_pairs, anchors)
        .standing_alone(&payloads)
        .mapping_phones_in_origin(&phones);
    let name_pass =
        names::NamePass::new(names, seed, scope, &number_keys, |name| raw_subs.apply(name, None));

    let id_pass = identifiers::IdentifierPass::new(
        &identifier_words,
        &short_identifier_words,
        &number_fakes,
        seed,
    );

    // A response's status code is protocol: no pass reaches it, whatever a
    // name spelling the status line elsewhere says. The digit pass never
    // reaches a display name the display pass masked: the mask holds its
    // numbers' fakes already, and mapping them again could lead back.
    let passes = |raw: &[u8], keep: Option<(&[u8], &[u8])>| -> Vec<u8> {
        let (head, tail) = raw.split_at(crate::rewrite::fields::status_code_end(raw));
        let at = |r: std::ops::Range<usize>| r.start - head.len()..r.end - head.len();
        // Protocol text is never anonymised: no pass reaches it.
        let protocol: Vec<_> = crate::rewrite::protocol_spans::protocol_spans(raw, scope.bodies())
            .into_iter()
            .filter(|p| p.start >= head.len())
            .map(at)
            .collect();
        let (masked, displays) = display_subs.apply_spans(tail);
        let mut digits = outside(&masked, &displays, |stretch| raw_subs.apply(stretch, keep));
        assert_eq!(digits.len(), tail.len(), "the masks and the number fakes keep every width");
        for span in &protocol {
            digits[span.clone()].copy_from_slice(&tail[span.clone()]);
        }
        // No name reaches an identifier's words (the identifier pass's alone)
        // nor an SDP origin's session numbers.
        let words = crate::rewrite::identifiers::words(raw).into_iter();
        let origins = crate::rewrite::origin_numbers(raw).into_iter();
        let spans = stretches::merged(
            words
                .chain(origins)
                .filter(|s| s.start >= head.len())
                .map(|s| (at(s), false))
                .chain(protocol.into_iter().map(|p| (p, true)))
                .collect(),
        );
        let ranges: Vec<_> = spans.iter().map(|(span, _)| span.clone()).collect();
        let (named, moved) =
            stretches::outside_tracked(&digits, &ranges, |stretch| name_pass.apply(stretch));
        let protected: Vec<_> =
            moved.into_iter().zip(&spans).filter(|(_, (_, p))| *p).map(|(m, _)| m).collect();
        let mut out = head.to_vec();
        out.extend(id_pass.apply_outside(&named, &protected));
        out
    };
    let rw_bytes = |raw: &[u8], keep: Option<(&[u8], &[u8])>| -> Vec<u8> {
        crate::encoded::rewrite(raw, &cx, |reading| passes(reading, keep))
    };
    let rw = |s: &str| -> String {
        String::from_utf8(rw_bytes(s.as_bytes(), None))
            .expect("same-charset substitutions keep UTF-8")
    };
    // A Call-ID or a tag in the model: rewritten exactly as the value of a
    // `Call-ID` line is on the wire.
    let rw_id = |s: &str| -> String {
        const LINE: &str = "Call-ID: ";
        let out = rw_bytes(format!("{LINE}{s}").as_bytes(), None);
        String::from_utf8(out[LINE.len()..].to_vec())
            .expect("same-charset substitutions keep UTF-8")
    };

    let original_call_ids: Vec<String> = flows.legs.iter().map(|l| l.call_id.clone()).collect();
    let rewrite = document::Rewrite { bytes: &rw_bytes, text: &rw, identifier: &rw_id };
    let redacted_parts = document::rewrite(flows, &rewrite, scope.bodies());

    guard_numeric_fields(flows)?;
    let merged_call_ids = count_merged_call_ids(&original_call_ids, flows);
    let (names, name_collisions) = (name_pass.entries(), name_pass.collisions());
    let names_written = [name_pass.fakes(), id_pass.fakes().to_vec()].concat();
    let fakes = Fakes::of(&entries, &names_written, &written, &spellings);
    flows.rewritten = Some("anonymised".to_string());
    Ok(Report {
        entries,
        collisions,
        merged_call_ids,
        uui_skipped,
        uui_redacted,
        names,
        name_collisions,
        fakes,
        redacted_parts,
    })
}

/// Whether a display name is left as found: `anonymous` (any case) or a
/// `urn:` service name, which name no one.
pub(crate) fn is_frozen_display(dn: &[u8]) -> bool {
    dn.eq_ignore_ascii_case(b"anonymous") || dn.starts_with(b"urn:")
}

#[cfg(test)]
mod control_probe_tests;
#[cfg(test)]
mod dialog_probe_tests;
#[cfg(test)]
mod digit_probe_tests;
#[cfg(test)]
mod digit_runs_probe_tests;
#[cfg(test)]
mod dotted_user_probe_tests;
#[cfg(test)]
mod encoded_place_probe_tests;
#[cfg(test)]
mod encoded_probe_tests;
#[cfg(test)]
mod framing_probe_tests;
#[cfg(test)]
mod kept_text_probe_tests;
#[cfg(test)]
mod protocol_text_probe_tests;
#[cfg(test)]
mod refusal_probe_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod vocabulary_probe_tests;
