//! [`LookupMap`]: a hash map read by key only.

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::hash::Hash;

#[expect(clippy::disallowed_types, reason = "the one std map, reached by key alone")]
type Inner<K, V> = std::collections::HashMap<K, V>;

/// A hash map under the std random seed, with no iteration.
#[derive(Clone)]
pub struct LookupMap<K, V> {
    inner: Inner<K, V>,
}

impl<K, V> LookupMap<K, V> {
    /// An empty map.
    pub fn new() -> Self {
        Self { inner: Inner::default() }
    }

    /// How many keys the map holds.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the map holds no key.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<K: Hash + Eq, V> LookupMap<K, V> {
    /// The value of `key`.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get(key)
    }

    /// The value of `key`, to change.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get_mut(key)
    }

    /// Whether `key` has a value.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.contains_key(key)
    }

    /// Set `key` to `value`; the value it held.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.inner.insert(key, value)
    }

    /// Take `key`'s value out.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.remove(key)
    }

    /// The slot of `key`, occupied or not.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        self.inner.entry(key)
    }
}

impl<K, V> Default for LookupMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> std::fmt::Debug for LookupMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupMap").field("len", &self.inner.len()).finish()
    }
}

/// Built from pairs in order: a later pair of a key replaces an earlier one.
impl<K: Hash + Eq, V> FromIterator<(K, V)> for LookupMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(pairs: I) -> Self {
        Self { inner: pairs.into_iter().collect() }
    }
}

impl<K: Hash + Eq, V> Extend<(K, V)> for LookupMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, pairs: I) {
        self.inner.extend(pairs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_reads_back_its_value_until_removed() {
        let mut m: LookupMap<String, u32> = LookupMap::new();
        assert!(m.is_empty());
        assert_eq!(m.insert("a".into(), 1), None);
        assert_eq!(m.insert("a".into(), 2), Some(1));
        *m.entry("b".into()).or_default() += 5;
        if let Some(v) = m.get_mut("a") {
            *v += 1;
        }
        assert_eq!((m.get("a"), m.get("b"), m.len()), (Some(&3), Some(&5), 2));
        assert!(m.contains_key("b"));
        assert_eq!(m.remove("b"), Some(5));
        assert!(!m.contains_key("b"));
    }

    #[test]
    fn the_last_pair_of_a_key_wins_and_debug_shows_the_length_only() {
        let m: LookupMap<&str, u32> = [("k", 1), ("k", 2), ("j", 3)].into_iter().collect();
        assert_eq!(m.get("k"), Some(&2));
        assert_eq!(format!("{m:?}"), "LookupMap { len: 2 }");
    }
}
