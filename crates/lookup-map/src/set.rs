//! [`LookupSet`]: a hash set asked about membership only.

use std::borrow::Borrow;
use std::hash::Hash;

#[expect(clippy::disallowed_types, reason = "the one std set, asked about membership alone")]
type Inner<T> = std::collections::HashSet<T>;

/// A hash set under the std random seed, with no iteration.
#[derive(Clone)]
pub struct LookupSet<T> {
    inner: Inner<T>,
}

impl<T> LookupSet<T> {
    /// An empty set.
    pub fn new() -> Self {
        Self { inner: Inner::default() }
    }

    /// How many members the set holds.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the set holds no member.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<T: Hash + Eq> LookupSet<T> {
    /// Add `value`; whether it was new.
    pub fn insert(&mut self, value: T) -> bool {
        self.inner.insert(value)
    }

    /// Whether `value` is a member.
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.contains(value)
    }

    /// Take `value` out; whether it was a member.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.remove(value)
    }
}

impl<T> Default for LookupSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> std::fmt::Debug for LookupSet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupSet").field("len", &self.inner.len()).finish()
    }
}

impl<T: Hash + Eq> FromIterator<T> for LookupSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        Self { inner: values.into_iter().collect() }
    }
}

impl<T: Hash + Eq> Extend<T> for LookupSet<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        self.inner.extend(values);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_follows_insert_and_remove() {
        let mut s: LookupSet<&str> = ["a"].into_iter().collect();
        assert!(s.insert("b"));
        assert!(!s.insert("a"));
        assert!(s.contains("b") && s.len() == 2);
        assert!(s.remove("a"));
        assert!(!s.contains("a"));
        assert_eq!(format!("{s:?}"), "LookupSet { len: 1 }");
    }
}
