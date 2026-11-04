//! The bytes one memo entry holds: every allocation it makes, rounded up to
//! the allocator's size classes, plus its share of the pool's indexes.
//! Accounted bytes, not resident memory: a hash table keeps its buckets after
//! its entries go, so a pool that was once full holds that capacity.

use std::mem::size_of;

use super::pool::INDEX_BYTES_PER_ENTRY;
use super::CancelEntry;
use crate::strategy::RouteParams;

/// The cells of a B-tree node (`std`'s `B = 6`).
const BTREE_CAPACITY: usize = 11;

/// The fewest cells a non-root B-tree node keeps.
const BTREE_MIN_CELLS: usize = 5;

/// One leaf node of a [`RouteParams`] map: its parent link, index and length,
/// then eleven key and eleven value cells.
const PARAMS_LEAF_BYTES: usize = 2 * size_of::<usize>() + 2 * BTREE_CAPACITY * size_of::<String>();

/// The bytes the memo accounts for `entry` under `key`: the key's shared
/// allocation (its reference counts and text), the target host, the branch,
/// each cookie map, and the index share.
pub(super) fn footprint(key: &str, entry: &CancelEntry) -> usize {
    [
        allocated(size_of::<[usize; 2]>().saturating_add(key.len())),
        allocated(entry.target.host.len()),
        allocated(entry.branch.len()),
        params(entry.stickiness.as_ref()),
        params(entry.minted.as_ref()),
        params(entry.via_cookie.as_ref()),
        INDEX_BYTES_PER_ENTRY,
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

/// A cookie map: its nodes and one allocation per name and value.
fn params(params: Option<&RouteParams>) -> usize {
    let Some(params) = params.filter(|p| !p.is_empty()) else { return 0 };
    let nodes =
        if params.len() <= BTREE_CAPACITY { 1 } else { params.len().div_ceil(BTREE_MIN_CELLS) };
    params
        .iter()
        .map(|(name, value)| allocated(name.len()).saturating_add(allocated(value.len())))
        .fold(nodes.saturating_mul(allocated(PARAMS_LEAF_BYTES)), usize::saturating_add)
}

/// The bytes the allocator hands out for a request of `n`: nothing for an
/// empty string, else a jemalloc size class — 16-byte steps up to 128, then
/// four classes per doubling.
fn allocated(n: usize) -> usize {
    match n {
        0 => 0,
        1..=128 => n.div_ceil(16).saturating_mul(16),
        _ => {
            let below = usize::BITS - (n - 1).leading_zeros() - 1;
            let step = 1usize << below.saturating_sub(2);
            n.div_ceil(step).saturating_mul(step)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_up_to_size_classes() {
        let classes: Vec<_> =
            [0, 1, 16, 17, 128, 129, 160, 161, 256, 257, 544, 1025].map(allocated).to_vec();
        assert_eq!(classes, [0, 16, 16, 32, 128, 160, 160, 192, 256, 320, 640, 1280]);
    }
}
