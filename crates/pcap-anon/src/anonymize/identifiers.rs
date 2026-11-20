//! The identifier pass: every word of a dialog or transaction identifier
//! ([`crate::rewrite::identifiers`]) maps to a fake of its shape — a letter
//! to a letter of its case, a digit to a digit, a hex word to a hex word —
//! keyed by the seed and drawn again on a collision, so the mapping is one
//! to one: no two Call-IDs, tags or branches merge, and every copy of one (an
//! `Event` dialog package's, a `Replaces`, an escaped URI header) maps with
//! it. It runs last, over what the other passes left — they never touch an
//! identifier — and replaces each word wherever it stands whole, case kept
//! exactly, outside the protocol counters and an SDP origin's session id and
//! version, which keep their +1 step (RFC 3264 §8). Vocabulary stays as found.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use aho_corasick::{AhoCorasick, MatchKind};

use crate::fake::{digest, Seed};
use crate::rewrite::fields::is_counter;

/// The branch magic cookie, after which a word starts though it is glued.
const COOKIE: &[u8] = b"z9hG4bK";

/// Draws in a word's own shape before the shape widens to letters and
/// digits, so a full shape still yields a fresh fake.
const SHAPED_DRAWS: u32 = 64;

/// Identifier words → their fakes.
pub(super) struct IdentifierPass {
    automaton: Option<AhoCorasick>,
    words: Vec<Vec<u8>>,
    fakes: Vec<Vec<u8>>,
}

impl IdentifierPass {
    /// The one-to-one mapping of `words` under `seed`, drawn shortest first,
    /// then in byte order, so it is the same whatever order discovery met
    /// them in. A word that ends with another word (a Call-ID a platform
    /// derived by gluing a prefix to its base) takes a fake of its prefix
    /// followed by that word's fake — a `short` word's being itself, as it
    /// stays — so the derivation survives. A word that is a number key takes
    /// that number's fake (`numbers`, the reserved fake space) while no other
    /// identifier holds it; no fake is a number key.
    pub(super) fn new(
        words: &HashSet<Vec<u8>>,
        short: &HashSet<Vec<u8>>,
        numbers: &HashMap<Vec<u8>, Vec<u8>>,
        seed: &Seed,
    ) -> IdentifierPass {
        let mut words: Vec<Vec<u8>> = words
            .iter()
            .filter(|w| !crate::fake::names::vocabulary(&String::from_utf8_lossy(w)))
            .cloned()
            .collect();
        words.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        // No fake is a word or a number key: this pass reads what the number
        // pass wrote, and a fake spelling a key would write a real number.
        let mut taken: HashSet<Vec<u8>> = words.iter().chain(numbers.keys()).cloned().collect();
        let mut fake_of: HashMap<Vec<u8>, Vec<u8>> =
            short.iter().map(|w| (w.clone(), w.clone())).collect();
        for word in &words {
            let number = numbers.get(word).filter(|f| {
                f.len() == word.len() && !taken.contains(*f) && !keeps_a_digit_run(word, f)
            });
            if let Some(fake) = number {
                taken.insert(fake.clone());
                fake_of.insert(word.clone(), fake.clone());
                continue;
            }
            let base =
                (1..word.len()).find_map(|k| fake_of.get(&word[k..]).map(|f| (k, f.clone())));
            let fake = (0..)
                .map(|draw| match &base {
                    Some((k, tail)) => [fake_word(seed, &word[..*k], draw), tail.clone()].concat(),
                    None => fake_word(seed, word, draw),
                })
                .find(|f| {
                    !taken.contains(f)
                        && !crate::fake::names::vocabulary(&String::from_utf8_lossy(f))
                        && !keeps_a_digit_run(word, f)
                })
                .expect("the draws never end");
            taken.insert(fake.clone());
            fake_of.insert(word.clone(), fake);
        }
        let fakes: Vec<Vec<u8>> = words.iter().map(|w| fake_of[w].clone()).collect();
        let automaton = (!words.is_empty()).then(|| {
            AhoCorasick::builder()
                .match_kind(MatchKind::Standard)
                .build(&words)
                .expect("identifier words are a bounded set of short patterns")
        });
        IdentifierPass { automaton, words, fakes }
    }

    /// Every fake the pass draws.
    pub(super) fn fakes(&self) -> &[Vec<u8>] {
        &self.fakes
    }

    /// [`Self::apply_outside`] over the whole of `input`.
    #[cfg(test)]
    pub(super) fn apply(&self, input: &[u8]) -> Vec<u8> {
        self.apply_outside(input, &[])
    }

    /// `input` with every whole occurrence of an identifier word replaced by
    /// its fake, the longest at each start, a counter's digits and every
    /// occurrence touching a `protected` span (protocol text) left alone.
    pub(super) fn apply_outside(&self, input: &[u8], protected: &[Range<usize>]) -> Vec<u8> {
        let Some(automaton) = &self.automaton else { return input.to_vec() };
        let mut found: Vec<(usize, usize, usize)> = automaton
            .find_overlapping_iter(input)
            .map(|m| (m.start(), m.end(), m.pattern().as_usize()))
            .filter(|&(start, end, id)| {
                !protected.iter().any(|p| p.start < end && start < p.end)
                    && opens_word(input, start)
                    && input.get(end).is_none_or(|b| !b.is_ascii_alphanumeric())
                    && !(self.words[id].iter().all(u8::is_ascii_digit)
                        && is_counter(input, start, end))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then((b.1 - b.0).cmp(&(a.1 - a.0))));
        let mut out = Vec::with_capacity(input.len());
        let mut at = 0;
        for (start, end, id) in found {
            if start < at {
                continue;
            }
            out.extend_from_slice(&input[at..start]);
            out.extend_from_slice(&self.fakes[id]);
            at = end;
        }
        out.extend_from_slice(&input[at..]);
        out
    }
}

/// Whether a word may start at `at`: no letter or digit before it, or a
/// percent escape, or a branch cookie that opens its own word.
fn opens_word(input: &[u8], at: usize) -> bool {
    if at == 0 || !input[at - 1].is_ascii_alphanumeric() {
        return true;
    }
    let escape = at >= 3 && input[at - 3] == b'%';
    let cookie = input[..at].ends_with(COOKIE)
        && (at == COOKIE.len() || !input[at - COOKIE.len() - 1].is_ascii_alphanumeric());
    escape || cookie
}

/// Whether `fake` leaves any four digits of `word` in a row where they stood:
/// a piece of a number the raw document spelt, which a kept short tail and a
/// drawn prefix digit could rebuild.
fn keeps_a_digit_run(word: &[u8], fake: &[u8]) -> bool {
    word.windows(4).zip(fake.windows(4)).any(|(w, f)| w.iter().all(u8::is_ascii_digit) && w == f)
}

/// Draw `draw` of a word's fake: in its own shape for the first draws, over
/// letters and digits after.
fn fake_word(seed: &Seed, word: &[u8], draw: u32) -> Vec<u8> {
    let hex = word.iter().all(u8::is_ascii_hexdigit);
    let mut message = b"identifier\0".to_vec();
    message.extend_from_slice(word);
    message.extend_from_slice(format!("#{draw}").as_bytes());
    let mut hash = Vec::with_capacity(word.len());
    let mut block = 0u32;
    while hash.len() < word.len() {
        let mut m = message.clone();
        m.extend_from_slice(format!("/{block}").as_bytes());
        hash.extend(digest(seed, &m));
        block += 1;
    }
    word.iter()
        .zip(hash)
        .map(|(&b, h)| {
            if draw >= SHAPED_DRAWS {
                return b"abcdefghijklmnopqrstuvwxyz0123456789"[usize::from(h) % 36];
            }
            match b {
                b'0'..=b'9' => b'0' + h % 10,
                b'a'..=b'f' if hex => b'a' + h % 6,
                b'A'..=b'F' if hex => b'A' + h % 6,
                b'a'..=b'z' => b'a' + h % 26,
                b'A'..=b'Z' => b'A' + h % 26,
                other => other,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thousand_ids_differing_in_a_short_run_stay_distinct_and_keep_their_shape() {
        let words: HashSet<Vec<u8>> = (0..1000).map(|i| format!("{i:05}").into_bytes()).collect();
        let pass = IdentifierPass::new(
            &words,
            &HashSet::new(),
            &HashMap::new(),
            &Seed::from_bytes([7; 32]),
        );
        let fakes: HashSet<&Vec<u8>> = pass.fakes().iter().collect();
        assert_eq!(fakes.len(), 1000, "one to one");
        assert!(pass.fakes().iter().all(|f| f.len() == 5));
        let text = b"Call-ID: 00042-x@h\r\nVia: SIP/2.0/UDP h:5060;branch=z9hG4bK00042\r\nCSeq: 00042 BYE\r\n";
        let out = String::from_utf8(pass.apply(text)).unwrap();
        assert!(
            !out.contains("00042-x") && !out.contains("z9hG4bK00042;") && !out.contains("bK00042"),
            "{out}"
        );
        assert!(out.contains("CSeq: 00042 BYE"), "a counter stays: {out}");
    }

    #[test]
    fn a_word_glued_onto_another_keeps_that_word_as_its_tail() {
        let words: HashSet<Vec<u8>> = ["12345fake", "1012345fake", "2212345fake", "4567test"]
            .into_iter()
            .map(|w| w.as_bytes().to_vec())
            .collect();
        let pass = IdentifierPass::new(
            &words,
            &HashSet::new(),
            &HashMap::new(),
            &Seed::from_bytes([7; 32]),
        );
        let fake = |w: &str| {
            String::from_utf8(pass.apply(format!("Call-ID: {w}\r\n").as_bytes())).unwrap()[9..]
                .trim_end()
                .to_string()
        };
        let base = fake("12345fake");
        assert_ne!(base, "12345fake");
        for derived in ["1012345fake", "2212345fake"] {
            let f = fake(derived);
            assert!(f.ends_with(&base) && f.len() == derived.len(), "{f} / {base}");
        }
        assert_ne!(fake("1012345fake"), fake("2212345fake"), "one to one");
    }

    #[test]
    fn a_word_glued_onto_a_short_word_never_keeps_a_raw_run_of_four_digits() {
        // `FAKEab2345` ends with the short opening word `345`: its fake keeps
        // that tail, and the prefix's last digit must never rebuild `2345`.
        let short: HashSet<Vec<u8>> = [b"345".to_vec()].into_iter().collect();
        for n in 0..200u32 {
            let word = format!("FAKE{n:03}ab2345").into_bytes();
            let words: HashSet<Vec<u8>> = [word.clone()].into_iter().collect();
            let pass =
                IdentifierPass::new(&words, &short, &HashMap::new(), &Seed::from_bytes([7; 32]));
            let fake = &pass.fakes()[0];
            assert!(fake.ends_with(b"345"), "{}", String::from_utf8_lossy(fake));
            assert!(!fake.ends_with(b"2345"), "{}", String::from_utf8_lossy(fake));
        }
    }

    #[test]
    fn a_word_that_is_a_number_key_takes_the_number_s_fake_while_it_is_free() {
        let words: HashSet<Vec<u8>> =
            [b"612345678".to_vec(), b"600000001".to_vec(), b"fakeword".to_vec()]
                .into_iter()
                .collect();
        // Two keys sharing one fake: the second word keeps the pass one to one.
        let numbers: HashMap<Vec<u8>, Vec<u8>> = [
            (b"612345678".to_vec(), b"000912345".to_vec()),
            (b"600000001".to_vec(), b"000912345".to_vec()),
        ]
        .into_iter()
        .collect();
        let pass =
            IdentifierPass::new(&words, &HashSet::new(), &numbers, &Seed::from_bytes([7; 32]));
        let fake = |w: &str| {
            String::from_utf8(pass.apply(format!("Call-ID: {w}\r\n").as_bytes())).unwrap()[9..]
                .trim_end()
                .to_string()
        };
        let (a, b) = (fake("600000001"), fake("612345678"));
        assert!(a == "000912345" || b == "000912345", "{a} / {b}");
        assert_ne!(a, b, "one to one");
    }

    #[test]
    fn no_identifier_fake_is_a_number_key() {
        // The number pass writes `4821` where `7735` stood and this pass then
        // reads `4821` as its word: a fake `7735` would write the real number
        // back. Every value invented.
        let words: HashSet<Vec<u8>> = [b"4821".to_vec()].into_iter().collect();
        let numbers: HashMap<Vec<u8>, Vec<u8>> =
            [(b"7735".to_vec(), b"4821".to_vec()), (b"4821".to_vec(), b"7735".to_vec())]
                .into_iter()
                .collect();
        for seed in 0..50u8 {
            let pass = IdentifierPass::new(
                &words,
                &HashSet::new(),
                &numbers,
                &Seed::from_bytes([seed; 32]),
            );
            assert!(!numbers.contains_key(&pass.fakes()[0]), "seed {seed}");
        }
    }

    #[test]
    fn no_fake_keeps_any_four_digit_window_of_its_word_in_place() {
        // `FAKEnnnab12345` ends with the short opening word `345`: the kept
        // tail and a drawn prefix digit could rebuild the window `2345`.
        let short: HashSet<Vec<u8>> = [b"345".to_vec()].into_iter().collect();
        for n in 0..200u32 {
            let word = format!("FAKE{n:03}ab12345").into_bytes();
            let words: HashSet<Vec<u8>> = [word.clone()].into_iter().collect();
            let pass =
                IdentifierPass::new(&words, &short, &HashMap::new(), &Seed::from_bytes([7; 32]));
            let fake = &pass.fakes()[0];
            for i in 0..=word.len() - 4 {
                let window = &word[i..i + 4];
                assert!(
                    !(window.iter().all(u8::is_ascii_digit) && window == &fake[i..i + 4]),
                    "{} keeps {}",
                    String::from_utf8_lossy(fake),
                    String::from_utf8_lossy(window)
                );
            }
        }
    }

    #[test]
    fn a_word_glued_onto_a_short_opening_word_keeps_it_as_its_tail() {
        let words: HashSet<Vec<u8>> =
            [b"1003".to_vec(), b"fakebase".to_vec()].into_iter().collect();
        let short: HashSet<Vec<u8>> = [b"3".to_vec()].into_iter().collect();
        let pass = IdentifierPass::new(&words, &short, &HashMap::new(), &Seed::from_bytes([7; 32]));
        let out =
            String::from_utf8(pass.apply(b"Call-ID: 1003-fakebase\r\nCall-ID: 3-fakebase\r\n"))
                .unwrap();
        let (derived, base) = out.split_once("\r\n").unwrap();
        let (derived, base) = (&derived[9..], base[9..].trim_end());
        assert!(base.starts_with("3-") && derived.ends_with(base), "{derived} / {base}");
        assert!(!derived.starts_with("100"), "the prefix maps: {derived}");
    }

    #[test]
    fn an_sdp_origin_spelling_an_identifier_word_keeps_its_session_id_and_version() {
        let words: HashSet<Vec<u8>> =
            [b"4417".to_vec(), b"4418".to_vec(), b"90217".to_vec()].into_iter().collect();
        let pass = IdentifierPass::new(
            &words,
            &HashSet::new(),
            &HashMap::new(),
            &Seed::from_bytes([7; 32]),
        );
        let text = b"Call-ID: 4417-90217@h\r\nVia: SIP/2.0/UDP h:5060;branch=z9hG4bK4418\r\n\r\n\
                     v=0\r\no=- 90217 4417 IN IP4 10.1.2.3\r\ns=-\r\n";
        let out = String::from_utf8(pass.apply(text)).unwrap();
        assert!(!out.contains("Call-ID: 4417-90217@"), "the Call-ID maps: {out}");
        assert!(!out.contains("bK4418"), "the branch maps: {out}");
        assert!(out.contains("o=- 90217 4417 IN IP4"), "the origin's numbers stay: {out}");
    }
}
