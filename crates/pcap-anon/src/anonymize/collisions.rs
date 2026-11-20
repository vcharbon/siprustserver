//! What the number mapping merged: fakes shared by several keys, a shorter
//! fake nested in a longer one, and the Call-IDs a shared fake made equal.
//! Statistics of a run, counted for the report, never a failure.

use std::collections::{HashMap, HashSet};

use super::{Collision, MappingEntry};
use crate::model::Flows;
use crate::rewrite::nested_pairs;

/// Legs whose rewritten Call-ID now equals another leg's although the
/// originals differed.
pub(super) fn count_merged_call_ids(originals: &[String], flows: &Flows) -> usize {
    let mut by_rewritten: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (orig, leg) in originals.iter().zip(&flows.legs) {
        by_rewritten.entry(&leg.call_id).or_default().insert(orig);
    }
    by_rewritten.values().filter(|set| set.len() > 1).map(HashSet::len).sum()
}

/// The collisions among the fakes of `entries`: shared fakes (in discovery
/// order of their first key) then nested pairs (shorter first, both in
/// discovery order). Keys of one width can only share, never nest.
pub(super) fn count_collisions(entries: &[MappingEntry]) -> Vec<Collision> {
    let by_fake = group_in_order(entries.iter().map(|e| (e.fake.as_str(), e.original.clone())));
    let mut collisions: Vec<Collision> = by_fake
        .iter()
        .filter(|(_, originals)| originals.len() > 1)
        .map(|(fake, originals)| Collision::Shared {
            fake: fake.to_string(),
            originals: originals.clone(),
        })
        .collect();
    let fakes: Vec<&str> = by_fake.iter().map(|(fake, _)| *fake).collect();
    collisions.extend(nested_pairs(&fakes).into_iter().map(|(shorter, longer)| {
        Collision::Nested { shorter: fakes[shorter].to_string(), longer: fakes[longer].to_string() }
    }));
    collisions
}

/// Discovery-ordered grouping of `items` by a key: each key's first position
/// and every item carrying it, in one pass.
pub(super) fn group_in_order<K: std::hash::Hash + Eq + Copy, V>(
    items: impl IntoIterator<Item = (K, V)>,
) -> Vec<(K, Vec<V>)> {
    let mut groups: Vec<(K, Vec<V>)> = Vec::new();
    let mut index: HashMap<K, usize> = HashMap::new();
    for (key, value) in items {
        match index.get(&key) {
            Some(&at) => groups[at].1.push(value),
            None => {
                index.insert(key, groups.len());
                groups.push((key, vec![value]));
            }
        }
    }
    groups
}
