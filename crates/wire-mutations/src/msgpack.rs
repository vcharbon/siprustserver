//! Structured mutations of a MessagePack body: truncation at an offset, a
//! marker byte overwritten, a length or number header written at its widest,
//! a run of one-element arrays nested in, a byte dropped. Every index is
//! taken modulo what the input holds, so any value applies to any seed.

use proptest::prelude::*;

/// One marker byte of every format family the specification names: nil, the
/// never-used byte, booleans, the fixed-width numbers, strings, binaries,
/// arrays, maps, extensions, and the fix forms at both ends of their range.
pub const MARKERS: &[u8] = &[
    0xc0, 0xc1, 0xc2, 0xc3, 0xcc, 0xcd, 0xce, 0xcf, 0xd0, 0xd3, 0xca, 0xcb, 0xd9, 0xda, 0xdb, 0xc4,
    0xc5, 0xc6, 0xdc, 0xdd, 0xde, 0xdf, 0xd4, 0xd8, 0xc7, 0xc9, 0x90, 0x9f, 0x80, 0x8f, 0xa0, 0xbf,
    0x00, 0x7f, 0xe0, 0xff,
];

/// Headers at their widest: lengths claiming far more than any body holds,
/// and the numbers at the bounds of their types.
pub const WIDE: &[&[u8]] = &[
    &[0xdb, 0xff, 0xff, 0xff, 0xff],
    &[0xc6, 0xff, 0xff, 0xff, 0xff],
    &[0xdd, 0xff, 0xff, 0xff, 0xff],
    &[0xdf, 0xff, 0xff, 0xff, 0xff],
    &[0xc9, 0xff, 0xff, 0xff, 0xff, 0x01],
    &[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    &[0xd3, 0x80, 0, 0, 0, 0, 0, 0, 0],
    &[0xcb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0],
    &[0xcb, 0xff, 0xf0, 0, 0, 0, 0, 0, 0],
];

/// How deep a [`Mutation::Nest`] nests: either side of a decoder's usual
/// bounds, and past what a small stack holds.
pub const NEST_DEPTHS: &[usize] = &[63, 64, 65, 128, 1024, 4096];

/// One-element array: the marker a nest repeats.
const FIXARRAY_ONE: u8 = 0x91;

/// One mutation, its indices free: [`apply`] reduces them against the input.
#[derive(Clone, Debug)]
pub enum Mutation {
    Truncate(usize),
    Marker { at: usize, marker: usize },
    Wide { at: usize, header: usize },
    Nest { at: usize, depth: usize },
    DropByte(usize),
}

/// `raw` with `m` applied.
pub fn apply(raw: &[u8], m: &Mutation) -> Vec<u8> {
    let mut out = raw.to_vec();
    let offset = |at: usize| at % (raw.len() + 1);
    match *m {
        Mutation::Truncate(at) => out.truncate(offset(at)),
        Mutation::Marker { at, marker } => {
            if let Some(byte) = out.get_mut(at % raw.len().max(1)) {
                *byte = MARKERS[marker % MARKERS.len()];
            }
        }
        Mutation::Wide { at, header } => {
            let at = offset(at);
            let header = WIDE[header % WIDE.len()];
            let end = (at + header.len()).min(out.len());
            out.splice(at..end, header.iter().copied());
        }
        Mutation::Nest { at, depth } => {
            let at = offset(at);
            let run = std::iter::repeat_n(FIXARRAY_ONE, NEST_DEPTHS[depth % NEST_DEPTHS.len()]);
            out.splice(at..at, run);
        }
        Mutation::DropByte(at) => {
            if !raw.is_empty() {
                out.remove(at % raw.len());
            }
        }
    }
    out
}

/// One of the mutations, any indices.
pub fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        3 => any::<usize>().prop_map(Mutation::Truncate),
        4 => (any::<usize>(), any::<usize>()).prop_map(|(at, marker)| Mutation::Marker { at, marker }),
        3 => (any::<usize>(), any::<usize>()).prop_map(|(at, header)| Mutation::Wide { at, header }),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(at, depth)| Mutation::Nest { at, depth }),
        2 => any::<usize>().prop_map(Mutation::DropByte),
    ]
}

/// The deterministic families of a corpus, each a list of mutations of one
/// seed: every truncation offset; at every offset, every marker, every wide
/// header, every nest depth and the byte dropped.
pub fn families(raw: &[u8]) -> Vec<(&'static str, Vec<Mutation>)> {
    let offsets = || 0..raw.len();
    vec![
        ("truncate", (0..=raw.len()).map(Mutation::Truncate).collect()),
        (
            "marker",
            offsets()
                .flat_map(|at| {
                    (0..MARKERS.len()).map(move |marker| Mutation::Marker { at, marker })
                })
                .collect(),
        ),
        (
            "wide",
            offsets()
                .flat_map(|at| (0..WIDE.len()).map(move |header| Mutation::Wide { at, header }))
                .collect(),
        ),
        (
            "nest",
            offsets()
                .flat_map(|at| {
                    (0..NEST_DEPTHS.len()).map(move |depth| Mutation::Nest { at, depth })
                })
                .collect(),
        ),
        ("drop", offsets().map(Mutation::DropByte).collect()),
    ]
}
