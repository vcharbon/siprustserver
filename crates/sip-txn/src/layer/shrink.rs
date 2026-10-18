//! Giving the memory of a drained table back to the allocator.

use std::collections::HashMap;

/// Below this many buckets a table is never shrunk: the saving is noise and a
/// small idle node would rehash on every sweep.
pub(super) const SHRINK_MIN_CAPACITY: usize = 1024;

/// Give a drained table back to the allocator: once it is under a quarter full
/// it is rehashed to twice its length. The factor-of-four gap is the hysteresis
/// that keeps a table oscillating around one size from rehashing every sweep.
pub(super) fn shrink_idle<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    let cap = map.capacity();
    if cap > SHRINK_MIN_CAPACITY && map.len() < cap / 4 {
        map.shrink_to(map.len() * 2);
    }
}
