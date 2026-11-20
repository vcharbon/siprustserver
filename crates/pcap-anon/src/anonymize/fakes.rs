//! Every fake one run wrote — the number fakes and every digit run the number
//! pass wrote, the name fakes whole and word by word (a name is faked word by
//! word), the address fakes — case left out: the set the leak check judges the
//! output by. A token of the output that is no fake of this run, no vocabulary
//! and no kept address names what the raw document named.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use sip_message::param_codec::decode_param;

use super::MappingEntry;

/// The fakes of one run.
#[derive(Debug, Default, Clone)]
pub struct Fakes {
    values: HashSet<Vec<u8>>,
    digits: HashSet<Vec<u8>>,
}

impl Fakes {
    /// The fakes of the number `entries`, of the `names` pass, the digit
    /// runs of every substitution the number pass `wrote`, and every
    /// spelling it wrote with separators or escapes (`spellings`), escapes
    /// decoded and read whole: the leak check reads a dotted one as an
    /// address (`10%2E00.97.42` spells `10.00.97.42`), and only that exact
    /// spelling is the run's.
    pub(super) fn of(
        entries: &[MappingEntry],
        names: &[Vec<u8>],
        wrote: &[Vec<u8>],
        spellings: &[Vec<u8>],
    ) -> Fakes {
        let mut values: HashSet<Vec<u8>> = spellings
            .iter()
            .map(|s| decode_param(&String::from_utf8_lossy(s)).to_ascii_lowercase().into_bytes())
            .collect();
        let whole =
            entries.iter().map(|e| e.fake.as_bytes()).chain(names.iter().map(Vec::as_slice));
        for fake in whole {
            let lower = fake.to_ascii_lowercase();
            for word in unescaped_words(&lower) {
                values.insert(word.to_vec());
            }
            // An address a name is faked into is read whole.
            let literal = |b: &u8| b.is_ascii_hexdigit() || matches!(b, b'.' | b':');
            for piece in lower.split(|b| !literal(b)) {
                let piece = String::from_utf8_lossy(piece);
                let piece = piece.trim_matches(['.', ':']);
                if piece.parse::<Ipv4Addr>().is_ok() || piece.parse::<Ipv6Addr>().is_ok() {
                    values.insert(piece.as_bytes().to_vec());
                }
            }
            values.insert(lower);
        }
        let digits = entries
            .iter()
            .map(|e| e.fake.as_bytes())
            .chain(wrote.iter().map(Vec::as_slice))
            .flat_map(|fake| fake.split(|b| !b.is_ascii_digit()))
            .filter(|run| !run.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        Fakes { values, digits }
    }

    /// Whether `token` is a fake this run wrote.
    pub fn contains(&self, token: &[u8]) -> bool {
        self.values.contains(&token.to_ascii_lowercase())
    }

    /// Whether a digit run is one the number pass wrote.
    pub fn wrote_digits(&self, run: &[u8]) -> bool {
        self.digits.contains(run)
    }
}

/// The alphanumeric words of `text`, a percent escape's three bytes
/// (`%40`) ending the word before them and belonging to none, as the names
/// pass fakes them.
fn unescaped_words(text: &[u8]) -> Vec<&[u8]> {
    let mut words = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i < text.len() {
        let escape = text[i] == b'%'
            && text.get(i + 1..i + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
        if escape || !text[i].is_ascii_alphanumeric() {
            words.extend(start.take().map(|s| &text[s..i]));
            i += if escape { 3 } else { 1 };
            continue;
        }
        start.get_or_insert(i);
        i += 1;
    }
    words.extend(start.map(|s| &text[s..]));
    words
}
