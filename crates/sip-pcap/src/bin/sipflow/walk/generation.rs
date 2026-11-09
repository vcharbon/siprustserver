//! Which walked documents stand: each capture at its current generation.
//!
//! An artifact store keeps a capture re-anonymised under its next generation's
//! name beside the generations before it, and its ingest record
//! (`ingest.json` in the capture directory) names the capture it `follows`.
//! A walked capture some walked record follows is superseded and left out.
//! A capture directory with no ingest record is judged by its name alone:
//! `<stem>` or `<stem>.anon`, then `<stem>.anon2`, `.anon3`…, the highest
//! generation of a stem the walk holds standing.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::capture_of;

/// The part of an ingest record the walk reads.
#[derive(Deserialize)]
struct Record {
    #[serde(default)]
    follows: Option<String>,
}

/// The ingest record of `document`'s capture directory, where one reads.
fn record_of(document: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(document.parent()?.join("ingest.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// `found` without its superseded documents; the `named` files all stay.
pub(crate) fn standing(found: Vec<PathBuf>, named: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let captures: Vec<String> = found.iter().map(|d| capture_of(d)).collect();
    let records: Vec<Option<Record>> = found.iter().map(|d| record_of(d)).collect();
    let followed: HashSet<&str> =
        records.iter().flatten().filter_map(|r| r.follows.as_deref()).collect();
    let mut current: BTreeMap<&str, u32> = BTreeMap::new();
    for (capture, _) in captures.iter().zip(&records).filter(|(_, r)| r.is_none()) {
        let (stem, generation) = generation_of(capture);
        let top = current.entry(stem).or_default();
        *top = (*top).max(generation);
    }
    found
        .into_iter()
        .zip(captures.iter().zip(&records))
        .filter(|(document, (capture, record))| {
            named.contains(document)
                || match record {
                    Some(_) => !followed.contains(capture.as_str()),
                    None => {
                        let (stem, generation) = generation_of(capture);
                        current.get(stem) == Some(&generation)
                    }
                }
        })
        .map(|(document, _)| document)
        .collect()
}

/// The stem a capture name's generations share, and the generation the name
/// is: `<stem>.anon<n>` (`n` from 2, no leading zero) is generation `n`, and
/// `<stem>.anon` and `<stem>` are generation 1.
fn generation_of(capture: &str) -> (&str, u32) {
    if let Some(at) = capture.rfind(".anon") {
        let digits = &capture[at + ".anon".len()..];
        if digits.is_empty() {
            return (&capture[..at], 1);
        }
        if !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n @ 2..) = digits.parse::<u32>() {
                return (&capture[..at], n);
            }
        }
    }
    (capture, 1)
}
