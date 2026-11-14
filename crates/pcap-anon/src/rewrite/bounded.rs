//! Whole-word substitution over the anonymiser's names: every occurrence of a
//! name whose neighbours are not word bytes (a letter, a digit, `-`, `_`),
//! longest first where occurrences overlap, then leftmost, compiled once per
//! run. A name is replaced where it stands inside a longer dotted one
//! (`alpha.zz` in `ims.alpha.zz`) and never inside another word
//! (`myalpha.zz`); a percent escape before a name (`%40alpha.zz`) ends the
//! word before it, and a name that opens or closes on a delimiter
//! (`"value"`, `>text<`) carries that boundary itself. A name is never
//! replaced inside a longer dotted quad (`33` in `203.0.113.33`): addresses
//! are the address pass's. Matching ignores ASCII case, and each replacement takes the
//! case of the occurrence it replaces, letter by letter.

use aho_corasick::AhoCorasick;

use crate::fake::address::ipv4_octets;

/// Names → same-length fakes, written lowercase.
pub struct BoundedSubs {
    automaton: Option<AhoCorasick>,
    fakes: Vec<Vec<u8>>,
    /// Whether pattern `i` opens and closes on a word byte: a name that
    /// opens or closes on a delimiter (`"value"`, `>text<`) carries that
    /// boundary itself.
    edges: Vec<(bool, bool)>,
}

/// A byte that continues a word, so a name it touches is part of that word.
fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Whether a word may start at `at`: the byte before is no word byte, or ends
/// a percent escape (`%40` before a host of an escaped URI header).
fn starts_word(input: &[u8], at: usize) -> bool {
    at == 0
        || !word_byte(input[at - 1])
        || (at >= 3
            && input[at - 3] == b'%'
            && input[at - 2].is_ascii_hexdigit()
            && input[at - 1].is_ascii_hexdigit())
}

/// Whether `start..end` lies inside a longer dotted quad (`33` in
/// `203.0.113.33`): an address literal is the address pass's, under its
/// scope, and no name is replaced inside one.
fn inside_quad(input: &[u8], start: usize, end: usize) -> bool {
    let quad_byte = |b: &&u8| b.is_ascii_digit() || **b == b'.';
    let from = start - input[..start].iter().rev().take_while(quad_byte).count();
    let to = end + input[end..].iter().take_while(quad_byte).count();
    (from < start || to > end)
        && std::str::from_utf8(&input[from..to])
            .is_ok_and(|run| ipv4_octets(run.trim_matches('.')).is_some())
}

impl BoundedSubs {
    /// Compile `(name, fake)` pairs; names are distinct, ignoring ASCII case,
    /// and non-empty.
    pub fn new(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> BoundedSubs {
        if pairs.is_empty() {
            return BoundedSubs { automaton: None, fakes: Vec::new(), edges: Vec::new() };
        }
        let automaton = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(pairs.iter().map(|(name, _)| name))
            .expect("names are a bounded set of short patterns");
        let edge = |name: &[u8]| {
            (
                name.first().is_some_and(|b| word_byte(*b)),
                name.last().is_some_and(|b| word_byte(*b)),
            )
        };
        BoundedSubs {
            automaton: Some(automaton),
            edges: pairs.iter().map(|(name, _)| edge(name)).collect(),
            fakes: pairs.into_iter().map(|(_, fake)| fake).collect(),
        }
    }

    /// The occurrences the rewrite applies, in input order: of every bounded
    /// occurrence, the longest at each start, each kept when it starts past
    /// the previous kept one.
    fn select(&self, input: &[u8]) -> Vec<(usize, usize, usize)> {
        let Some(automaton) = &self.automaton else { return Vec::new() };
        let mut found: Vec<(usize, usize, usize)> = automaton
            .find_overlapping_iter(input)
            .filter(|m| {
                let (opens, closes) = self.edges[m.pattern().as_usize()];
                (!opens || starts_word(input, m.start()))
                    && (!closes || input.get(m.end()).is_none_or(|&b| !word_byte(b)))
                    && !inside_quad(input, m.start(), m.end())
            })
            .map(|m| (m.start(), m.end(), m.pattern().as_usize()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then((b.1 - b.0).cmp(&(a.1 - a.0))));
        let mut chosen = Vec::with_capacity(found.len());
        let mut end = 0;
        for occurrence in found {
            if occurrence.0 >= end {
                end = occurrence.1;
                chosen.push(occurrence);
            }
        }
        chosen
    }

    /// `input` with every chosen occurrence replaced by its fake.
    pub fn apply(&self, input: &[u8]) -> Vec<u8> {
        let chosen = self.select(input);
        if chosen.is_empty() {
            return input.to_vec();
        }
        let mut out = Vec::with_capacity(input.len());
        let mut at = 0;
        for (start, end, id) in chosen {
            out.extend_from_slice(&input[at..start]);
            out.extend(self.fakes[id].iter().zip(&input[start..end]).map(|(&fake, &found)| {
                if found.is_ascii_uppercase() {
                    fake.to_ascii_uppercase()
                } else {
                    fake.to_ascii_lowercase()
                }
            }));
            at = end;
        }
        out.extend_from_slice(&input[at..]);
        out
    }

    /// The index (in the pairs `new` took) of every name the rewrite would
    /// replace in `input`, in input order.
    pub fn found(&self, input: &[u8]) -> Vec<usize> {
        self.select(input).into_iter().map(|(_, _, id)| id).collect()
    }

    /// [`Self::found`] with where each occurrence starts: `(start, index)`.
    pub fn found_at(&self, input: &[u8]) -> Vec<(usize, usize)> {
        self.select(input).into_iter().map(|(start, _, id)| (start, id)).collect()
    }
}

#[cfg(test)]
mod tests;
