//! Multi-pattern matching over the anonymizer's mapping: the substitution sets
//! compiled once per run ([`DigitSubs`], [`DisplaySubs`]) and the one
//! left-to-right pass that applies them, linear in the bytes scanned whatever
//! the number of entries, and the nested-fake detection over the fakes
//! themselves ([`nested_pairs`]). The names pass matches whole words
//! ([`bounded`]) and address literals ([`addresses`]).
//!
//! Digit keys match longest-first: of two overlapping occurrences the longer
//! key is the identity, whichever starts first — a short code spelling the
//! last digit of a CC and the first of an NSN never splits that NSN — except
//! that a weak key (an unrecognised run) yields to a recognised one it
//! partially overlaps, and the chosen occurrences never cascade: they are
//! taken from the input, so a fake is never re-matched and a nested key pair
//! rewrites correctly. The CC digits behind a `+` are an anchor: an
//! occurrence that partially overlaps them without containing them yields,
//! so no key spells the end of a CC and the head of what follows. The
//! exemptions are the message's protocol counters, which are framing, not
//! identity: the CSeq value (`keep`) — the digit run that is the sequence
//! number written before its method (`CSeq: 41001 INVITE`, `RAck: 1 41001
//! INVITE`) — and every counter named by its field (`fields`: a port, a
//! length, a timer, the media lines of a description, an SDP `o=`
//! sess-version whatever key it spells, so the version keeps its +1 step,
//! RFC 3264 §8, and a sess-id but where a phone-width key spells it: that is
//! the number, mapped alike in every offer). The same digits as a user-part,
//! Call-ID or display run still map. Display names
//! match in two forms — quoted, or unquoted and directly followed by optional
//! whitespace then `<` — the longest name first at every position.

use std::collections::HashSet;

use aho_corasick::{AhoCorasick, MatchKind};

use crate::identities::PHONE_WIDTH;

pub mod addresses;
pub mod bounded;
pub(crate) mod fields;
pub(crate) mod identifiers;
pub(crate) mod protocol_spans;

/// Substitution pairs in the priority the scan tries them: longest key first,
/// then bytewise, so pattern ids ARE the priority order.
fn prioritized(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = pairs;
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    pairs
}

/// Digit keys → same-width fakes, compiled once per run. The anonymizer's
/// raw-byte set also carries the PASSporT payload segments beside the digit
/// keys: a segment matched whole is consumed whole, so the digit keys inside
/// its base64 text never match.
pub struct DigitSubs {
    /// Every occurrence of every key (overlapping); pattern id `i` maps to
    /// `fakes[i]`.
    automaton: AhoCorasick,
    fakes: Vec<Vec<u8>>,
    /// Whether pattern `i` is weak: it yields to a strong occurrence it
    /// partially overlaps (an unrecognised run beside a recognised number),
    /// but not to one it contains (its composed form carries that one).
    weak: Vec<bool>,
    /// Whether pattern `i` is an anchor: CC digits that, right behind a `+`,
    /// every occurrence partially overlapping them yields to; mapped to
    /// themselves.
    anchor: Vec<bool>,
    /// The key of pattern `i`.
    keys: Vec<Vec<u8>>,
    /// Whether pattern `i` matches only standing alone, no letter or digit
    /// beside it ([`DigitSubs::standing_alone`]).
    alone: Vec<bool>,
    /// Whether pattern `i` is a phone number, mapped even as an `o=` session
    /// id ([`DigitSubs::mapping_phones_in_origin`]).
    phone: Vec<bool>,
}

/// One chosen occurrence: `(start, end, pattern id)`.
type Occurrence = (usize, usize, usize);

impl DigitSubs {
    /// Compile `(key, fake)` pairs of equal width, all strong; keys are
    /// distinct and non-empty.
    pub fn new(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> DigitSubs {
        DigitSubs::with_weak(pairs, Vec::new())
    }

    /// Compile strong and weak `(key, fake)` pairs (struct doc).
    pub fn with_weak(strong: Vec<(Vec<u8>, Vec<u8>)>, weak: Vec<(Vec<u8>, Vec<u8>)>) -> DigitSubs {
        DigitSubs::anchored(strong, weak, Vec::new())
    }

    /// Compile strong and weak `(key, fake)` pairs and the anchors (struct
    /// doc); an anchor is distinct from every key.
    pub fn anchored(
        strong: Vec<(Vec<u8>, Vec<u8>)>,
        weak: Vec<(Vec<u8>, Vec<u8>)>,
        anchors: Vec<Vec<u8>>,
    ) -> DigitSubs {
        let mut pairs: Vec<(Vec<u8>, Vec<u8>, bool, bool)> = strong
            .into_iter()
            .map(|(from, to)| (from, to, false, false))
            .chain(weak.into_iter().map(|(from, to)| (from, to, true, false)))
            .chain(anchors.into_iter().map(|cc| (cc.clone(), cc, false, true)))
            .collect();
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
        let automaton = AhoCorasick::builder()
            .match_kind(MatchKind::Standard)
            .build(pairs.iter().map(|(from, _, _, _)| from))
            .expect("digit keys are a bounded set of short patterns");
        let weak = pairs.iter().map(|(_, _, weak, _)| *weak).collect();
        let anchor = pairs.iter().map(|(_, _, _, anchor)| *anchor).collect();
        let alone = vec![false; pairs.len()];
        let phone = vec![false; pairs.len()];
        let (keys, fakes) = pairs.into_iter().map(|(from, to, _, _)| (from, to)).unzip();
        DigitSubs { automaton, fakes, weak, anchor, keys, alone, phone }
    }

    /// Map `keys`, phone numbers, where one is an `o=` session id whole:
    /// a session id that spells a phone number is that number.
    pub fn mapping_phones_in_origin(mut self, keys: &HashSet<Vec<u8>>) -> DigitSubs {
        self.phone = self.keys.iter().map(|k| keys.contains(k)).collect();
        self
    }

    /// Match `keys` only where they stand alone: an encoded payload (a hex
    /// User-to-User, a PASSporT segment) is rewritten where it is written
    /// whole, never where its characters spell part of something else.
    pub fn standing_alone(mut self, keys: &HashSet<Vec<u8>>) -> DigitSubs {
        self.alone = self.keys.iter().map(|k| keys.contains(k)).collect();
        self
    }

    /// The non-overlapping occurrences the rewrite applies, in input order:
    /// of every occurrence of every key, one partially overlapping an anchor
    /// behind a `+` yielding, a weak one yielding to a strong one it
    /// partially overlaps, then longest first, then leftmost, each kept when
    /// no chosen one overlaps it.
    fn select(&self, input: &[u8]) -> Vec<Occurrence> {
        let mut found: Vec<Occurrence> = self
            .automaton
            .find_overlapping_iter(input)
            .map(|m| (m.start(), m.end(), m.pattern().as_usize()))
            .filter(|&(start, end, id)| {
                !self.alone[id]
                    || ((start == 0 || !input[start - 1].is_ascii_alphanumeric())
                        && input.get(end).is_none_or(|b| !b.is_ascii_alphanumeric()))
            })
            .collect();
        if self.anchor.iter().any(|a| *a) {
            let anchors: Vec<Occurrence> = found
                .iter()
                .copied()
                .filter(|&(start, _, id)| self.anchor[id] && start > 0 && input[start - 1] == b'+')
                .collect();
            found.retain(|&(start, end, id)| {
                !self.anchor[id]
                    && !anchors
                        .iter()
                        .any(|&(s, e, _)| s < end && start < e && !(start <= s && e <= end))
            });
            found.extend(anchors);
        }
        if found.len() <= 1 {
            return found;
        }
        if self.weak.iter().any(|w| *w) {
            let strong: Vec<Occurrence> =
                found.iter().copied().filter(|(_, _, id)| !self.weak[*id]).collect();
            found.retain(|&(start, end, id)| {
                !self.weak[id]
                    || !strong
                        .iter()
                        .any(|&(s, e, _)| s < end && start < e && !(start <= s && e <= end))
            });
        }
        found.sort_unstable_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)).then(a.cmp(b)));
        let mut taken = vec![false; input.len()];
        let mut chosen = Vec::with_capacity(found.len());
        for (start, end, id) in found {
            if taken[start..end].iter().any(|t| *t) {
                continue;
            }
            taken[start..end].fill(true);
            chosen.push((start, end, id));
        }
        chosen.sort_unstable();
        chosen
    }

    /// Rewrite every key occurrence in `input` to its fake, except inside a
    /// protocol counter: the CSeq value `keep` = `(seq, method)` states, a
    /// counter by its field (`fields::is_counter`: a port, a length, an SDP
    /// media line, an `o=` session number …). Each digit run is judged once:
    /// the verdict holds for every match starting before the run's end, since
    /// a key never spans a non-digit.
    pub fn apply(&self, input: &[u8], keep: Option<(&[u8], &[u8])>) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut last = 0;
        // The run's end, whether it is a protocol counter, and whether it is
        // an `o=` session id.
        let mut judged: Option<(usize, bool, bool)> = None;
        for (start, end, id) in self.select(input) {
            let (_, counter, sess_id) = match judged {
                Some(verdict) if start < verdict.0 => verdict,
                _ => {
                    let (run_end, counter) = protocol_counter_run_at(input, start, keep);
                    let run_start = digit_run_start(input, start);
                    let sess_id =
                        origin_field(input, run_start, run_end) == Some(OriginField::SessId);
                    judged = Some((run_end, counter, sess_id));
                    (run_end, counter, sess_id)
                }
            };
            // A session id spelling a phone number is that number: mapped,
            // the same in every offer, so the origin's continuity holds.
            let counter = counter && !(sess_id && self.phone[id] && end - start >= PHONE_WIDTH);
            // A percent escape's hex digits spell a byte, never a number.
            let escape = fields::in_escape(input, start) || fields::in_escape(input, end - 1);
            // An identifier is the identifier pass's alone.
            let identifier = identifiers::is_identifier(input, start, end);
            if counter || escape || identifier {
                // Its bytes ride verbatim with the gap before the next match.
                continue;
            }
            out.extend_from_slice(&input[last..start]);
            out.extend_from_slice(&self.fakes[id]);
            last = end;
        }
        out.extend_from_slice(&input[last..]);
        out
    }

    /// Where the first chosen key occurrence in `input` starts, if any.
    pub fn earliest(&self, input: &[u8]) -> Option<usize> {
        self.select(input).first().map(|(start, _, _)| *start)
    }

    /// Like [`DigitSubs::apply`] over an all-digit display run: every byte
    /// no key consumed is emitted as `0`, so the run lands wholly in the
    /// reserved space.
    pub fn apply_zeroing(&self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut last = 0;
        for (start, end, id) in self.select(input) {
            out.resize(out.len() + start - last, b'0');
            out.extend_from_slice(&self.fakes[id]);
            last = end;
        }
        out.resize(out.len() + input.len() - last, b'0');
        out
    }
}

/// The end of the digit run around `input[i]`, and whether that run is a
/// protocol counter: the CSeq value `cseq` states, or a counter by its field
/// ([`fields::is_counter`]).
pub(crate) fn protocol_counter_run_at(
    input: &[u8],
    i: usize,
    cseq: Option<(&[u8], &[u8])>,
) -> (usize, bool) {
    let start = digit_run_start(input, i);
    let end = input[i..].iter().position(|b| !b.is_ascii_digit()).map_or(input.len(), |p| i + p);
    let stated = cseq.is_some_and(|cseq| cseq_value_run_at(input, i, cseq).1);
    (end, stated || fields::is_counter(input, start, end))
}

/// The start of the digit run around `input[i]`.
fn digit_run_start(input: &[u8], i: usize) -> usize {
    input[..i].iter().rposition(|b| !b.is_ascii_digit()).map_or(0, |p| p + 1)
}

/// Which `o=` field `input[start..end]` is whole: [`OriginField`], the
/// second or third space-separated field of `o=<username> <sess-id>
/// <sess-version> <nettype> …` (RFC 8866 §5.2).
pub(crate) fn origin_field(input: &[u8], start: usize, end: usize) -> Option<OriginField> {
    if start == 0 || input[start - 1] != b' ' || input.get(end) != Some(&b' ') {
        return None;
    }
    let line_start = input[..start].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let fields = input[line_start..start].strip_prefix(b"o=")?;
    match fields.split(|b| *b == b' ').filter(|f| !f.is_empty()).count() {
        1 => Some(OriginField::SessId),
        2 => Some(OriginField::SessVersion),
        _ => None,
    }
}

/// The sess-id and sess-version of every SDP `o=` line of `input`: protocol
/// numbers no name reaches (a digit-only name, an inter-operator id, would
/// break the version's step).
pub(crate) fn origin_numbers(input: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < input.len() {
        let rest = &input[at..];
        let line = rest.iter().position(|b| matches!(b, b'\r' | b'\n')).unwrap_or(rest.len());
        if rest.starts_with(b"o=") {
            let mut start = at + 2;
            for (i, field) in input[at + 2..at + line].split(|b| *b == b' ').enumerate() {
                if (i == 1 || i == 2) && !field.is_empty() {
                    out.push(start..start + field.len());
                }
                start += field.len() + 1;
            }
        }
        at += line + 1;
    }
    out
}

/// The two numeric fields of an SDP `o=` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OriginField {
    SessId,
    SessVersion,
}

/// The end of the digit run around `input[i]`, and whether that run is `seq`
/// followed (after SP/HTAB) by `method` as a whole token.
fn cseq_value_run_at(input: &[u8], i: usize, (seq, method): (&[u8], &[u8])) -> (usize, bool) {
    let start = input[..i].iter().rposition(|b| !b.is_ascii_digit()).map_or(0, |p| p + 1);
    let end = input[i..].iter().position(|b| !b.is_ascii_digit()).map_or(input.len(), |p| i + p);
    if &input[start..end] != seq {
        return (end, false);
    }
    let mut j = end;
    while input.get(j).is_some_and(|b| *b == b' ' || *b == b'\t') {
        j += 1;
    }
    let follows = input[j..].len() >= method.len()
        && input[j..j + method.len()].eq_ignore_ascii_case(method)
        && !input.get(j + method.len()).is_some_and(|b| b.is_ascii_alphanumeric());
    (end, follows)
}

/// Raw display names → their masked forms, compiled once per run.
pub struct DisplaySubs {
    /// Standard (overlapping) over the names, so every name occurring at a
    /// position is enumerated; pattern id `i` maps to `names[i]`/`masks[i]`.
    automaton: AhoCorasick,
    names: Vec<Vec<u8>>,
    masks: Vec<Vec<u8>>,
}

/// One way a name can be rewritten at a position: the quoted form takes
/// precedence over the unquoted one for the same name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    Quoted,
    Unquoted,
}

impl DisplaySubs {
    /// Compile `(name, mask)` pairs; names are distinct and non-empty.
    pub fn new(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> DisplaySubs {
        let pairs = prioritized(pairs);
        let automaton = AhoCorasick::builder()
            .match_kind(MatchKind::Standard)
            .build(pairs.iter().map(|(from, _)| from))
            .expect("display names are a bounded set of short patterns");
        let (names, masks) = pairs.into_iter().unzip();
        DisplaySubs { automaton, names, masks }
    }

    /// Rewrite every display-name occurrence in `input`: the quoted form
    /// `"name"`, or the unquoted form when `name` is directly followed by
    /// optional whitespace and `<`. Runs before the digit pass, which never
    /// reaches a name it rewrote: the mask already holds the fakes of the
    /// numbers the name spelt ([`Self::apply_spans`]).
    pub fn apply(&self, input: &[u8]) -> Vec<u8> {
        self.apply_spans(input).0
    }

    /// [`Self::apply`], with the span of every name it rewrote (the masks
    /// keep each name's width, so a span stands where the name stood).
    pub fn apply_spans(&self, input: &[u8]) -> (Vec<u8>, Vec<std::ops::Range<usize>>) {
        let mut spans = Vec::new();
        if self.names.is_empty() {
            return (input.to_vec(), spans);
        }
        // Every name occurrence, by start position then priority.
        let mut occurrences: Vec<(usize, usize)> = self
            .automaton
            .find_overlapping_iter(input)
            .map(|m| (m.start(), m.pattern().as_usize()))
            .collect();
        occurrences.sort_unstable();

        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;
        let mut cursor = 0;
        while i < input.len() {
            while cursor < occurrences.len() && occurrences[cursor].0 < i {
                cursor += 1;
            }
            // A name can only be rewritten at `i` if one starts at `i`
            // (unquoted) or at `i + 1` (quoted): copy up to the next chance.
            let next_chance = match occurrences.get(cursor) {
                Some(&(start, _)) => start.saturating_sub(1).max(i),
                None => input.len(),
            };
            if next_chance > i {
                out.extend_from_slice(&input[i..next_chance]);
                i = next_chance;
                continue;
            }
            match self.choose(input, i, &occurrences[cursor..]) {
                Some((id, Form::Quoted)) => {
                    out.push(b'"');
                    spans.push(out.len()..out.len() + self.masks[id].len());
                    out.extend_from_slice(&self.masks[id]);
                    out.push(b'"');
                    i += self.names[id].len() + 2;
                }
                Some((id, Form::Unquoted)) => {
                    spans.push(out.len()..out.len() + self.masks[id].len());
                    out.extend_from_slice(&self.masks[id]);
                    i += self.names[id].len();
                }
                None => {
                    out.push(input[i]);
                    i += 1;
                }
            }
        }
        (out, spans)
    }

    /// The first name, in priority order, that rewrites at `i`: the quoted
    /// form (the name starts at `i + 1` between two `"`) before the unquoted
    /// form (the name starts at `i`, then optional whitespace, then `<`).
    fn choose(&self, input: &[u8], i: usize, ahead: &[(usize, usize)]) -> Option<(usize, Form)> {
        ahead
            .iter()
            .take_while(|&&(start, _)| start <= i + 1)
            .filter_map(|&(start, id)| {
                let len = self.names[id].len();
                if start == i + 1 {
                    let quoted = input[i] == b'"' && input.get(start + len) == Some(&b'"');
                    quoted.then_some((id, Form::Quoted))
                } else {
                    let mut j = i + len;
                    while input.get(j).is_some_and(|b| *b == b' ' || *b == b'\t') {
                        j += 1;
                    }
                    (input.get(j) == Some(&b'<')).then_some((id, Form::Unquoted))
                }
            })
            .min()
    }
}

/// Which of `needles` is a substring of a longer `needle`, as sorted
/// `(shorter, longer)` index pairs: one pass of an overlapping automaton over
/// the needles themselves, not a pairwise scan.
pub fn nested_pairs(needles: &[&str]) -> Vec<(usize, usize)> {
    let automaton = AhoCorasick::builder()
        .match_kind(MatchKind::Standard)
        .build(needles)
        .expect("fakes are a bounded set of short patterns");
    let mut pairs: Vec<(usize, usize)> = needles
        .iter()
        .enumerate()
        .flat_map(|(longer, hay)| {
            automaton
                .find_overlapping_iter(hay.as_bytes())
                .map(|m| m.pattern().as_usize())
                .filter(|&shorter| needles[shorter].len() < hay.len())
                .map(move |shorter| (shorter, longer))
                .collect::<Vec<_>>()
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

#[cfg(test)]
mod tests;
