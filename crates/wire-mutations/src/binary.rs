//! Structured mutations of a binary frame: truncation at an offset, a byte
//! dropped or replaced by a value a length or type marker gives meaning to,
//! and a run of bytes stating an extreme length or number inserted. Every
//! index is taken modulo what the input holds, so any value applies to any
//! seed.

use proptest::prelude::*;

/// Byte values a binary format gives meaning to: the bounds, and the msgpack
/// markers of nil, the booleans, the sized integers, strings, binaries and
/// arrays.
pub const BYTES: &[u8] = &[
    0x00, 0x01, 0x7f, 0x80, 0xff, 0xc0, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xcc, 0xcd, 0xce, 0xcf, 0xd0,
    0xd3, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0x90, 0x9f, 0xa0, 0xbf,
];

/// Runs inserted whole: a length or number at its type's extreme (the
/// largest 32-bit string, binary and array lengths, the 64-bit bounds), a
/// length prefix past any frame, and an empty array.
pub const RUNS: &[&[u8]] = &[
    &[0xdb, 0xff, 0xff, 0xff, 0xff],
    &[0xc6, 0xff, 0xff, 0xff, 0xff],
    &[0xdd, 0xff, 0xff, 0xff, 0xff],
    &[0xdc, 0xff, 0xff],
    &[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    &[0xd3, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    &[0xff, 0xff, 0xff, 0xff],
    &[0x00, 0x00, 0x00, 0x00],
    &[0x90],
];

/// One mutation, its indices free: [`apply`] reduces them against the input.
#[derive(Clone, Debug)]
pub enum Mutation {
    Truncate(usize),
    DropByte(usize),
    Replace { at: usize, value: usize },
    Insert { at: usize, run: usize },
}

/// `raw` with `m` applied.
pub fn apply(raw: &[u8], m: &Mutation) -> Vec<u8> {
    let mut out = raw.to_vec();
    match *m {
        Mutation::Truncate(at) => out.truncate(at % (raw.len() + 1)),
        Mutation::DropByte(at) => {
            if !raw.is_empty() {
                out.remove(at % raw.len());
            }
        }
        Mutation::Replace { at, value } => {
            if !raw.is_empty() {
                out[at % raw.len()] = BYTES[value % BYTES.len()];
            }
        }
        Mutation::Insert { at, run } => {
            let at = at % (raw.len() + 1);
            out.splice(at..at, RUNS[run % RUNS.len()].iter().copied());
        }
    }
    out
}

/// One of the mutations, any indices.
pub fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        3 => any::<usize>().prop_map(Mutation::Truncate),
        2 => any::<usize>().prop_map(Mutation::DropByte),
        4 => (any::<usize>(), any::<usize>()).prop_map(|(at, value)| Mutation::Replace { at, value }),
        3 => (any::<usize>(), any::<usize>()).prop_map(|(at, run)| Mutation::Insert { at, run }),
    ]
}

/// The deterministic families of the structured corpus of one seed: every
/// truncation offset, every byte dropped, every byte replaced by every
/// [`BYTES`] value, every [`RUNS`] run at every offset.
pub fn families(raw: &[u8]) -> Vec<(&'static str, Vec<Mutation>)> {
    vec![
        ("truncate", (0..=raw.len()).map(Mutation::Truncate).collect()),
        ("drop", (0..raw.len()).map(Mutation::DropByte).collect()),
        (
            "replace",
            (0..raw.len())
                .flat_map(|at| (0..BYTES.len()).map(move |value| Mutation::Replace { at, value }))
                .collect(),
        ),
        (
            "insert",
            (0..=raw.len())
                .flat_map(|at| (0..RUNS.len()).map(move |run| Mutation::Insert { at, run }))
                .collect(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_family_reaches_every_offset() {
        let raw = [0x93, 0x02, 0x01, 0x05];
        let families = families(&raw);
        let count = |name: &str| families.iter().find(|(n, _)| *n == name).map(|(_, m)| m.len());
        assert_eq!(count("truncate"), Some(5));
        assert_eq!(count("drop"), Some(4));
        assert_eq!(count("replace"), Some(4 * BYTES.len()));
        assert_eq!(count("insert"), Some(5 * RUNS.len()));
        assert_eq!(apply(&raw, &Mutation::Replace { at: 1, value: 4 }), [0x93, 0xff, 0x01, 0x05]);
        assert_eq!(
            apply(&raw, &Mutation::Insert { at: 4, run: 8 }),
            [0x93, 0x02, 0x01, 0x05, 0x90]
        );
    }
}
