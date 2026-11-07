//! The identity of a client transaction (RFC 3261 §17.1.3): the branch of the
//! request this node sent, which a response echoes. A branch compares ignoring
//! ASCII case (§7.3.1), so a response whose branch a hop re-cased matches its
//! transaction. The collections here file client transactions by that identity
//! and are looked up by a borrowed branch without allocating.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use sip_message::header::{hash_branch, same_branch};

use super::shrink::shrink_idle;

/// A client transaction's branch as this node sent it.
#[derive(Debug, Clone)]
pub(super) struct ClientTxnKey(String);

impl ClientTxnKey {
    pub(super) fn branch(&self) -> &str {
        &self.0
    }
}

/// A branch a client collection is looked up by, owned or borrowed alike.
pub(super) trait BranchIdentity {
    fn branch(&self) -> &str;
}

impl BranchIdentity for ClientTxnKey {
    fn branch(&self) -> &str {
        &self.0
    }
}

impl BranchIdentity for &str {
    fn branch(&self) -> &str {
        self
    }
}

impl PartialEq for dyn BranchIdentity + '_ {
    fn eq(&self, other: &Self) -> bool {
        same_branch(self.branch(), other.branch())
    }
}

impl Eq for dyn BranchIdentity + '_ {}

impl Hash for dyn BranchIdentity + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_branch(self.branch(), state);
    }
}

impl<'a> Borrow<dyn BranchIdentity + 'a> for ClientTxnKey {
    fn borrow(&self) -> &(dyn BranchIdentity + 'a) {
        self
    }
}

impl PartialEq for ClientTxnKey {
    fn eq(&self, other: &Self) -> bool {
        same_branch(&self.0, &other.0)
    }
}

impl Eq for ClientTxnKey {}

impl Hash for ClientTxnKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_branch(&self.0, state);
    }
}

fn lookup<'a>(branch: &'a &'a str) -> &'a (dyn BranchIdentity + 'a) {
    branch
}

/// Values filed by client transaction branch.
pub(super) struct ClientMap<V>(HashMap<ClientTxnKey, V>);

impl<V> ClientMap<V> {
    pub(super) fn new() -> Self {
        Self(HashMap::new())
    }

    pub(super) fn get(&self, branch: &str) -> Option<&V> {
        self.0.get(lookup(&branch))
    }

    pub(super) fn get_mut(&mut self, branch: &str) -> Option<&mut V> {
        self.0.get_mut(lookup(&branch))
    }

    pub(super) fn contains_key(&self, branch: &str) -> bool {
        self.0.contains_key(lookup(&branch))
    }

    pub(super) fn remove(&mut self, branch: &str) -> Option<V> {
        self.0.remove(lookup(&branch))
    }

    /// Files `value` under `branch`; the value a branch equal to it held.
    pub(super) fn insert(&mut self, branch: String, value: V) -> Option<V> {
        self.0.insert(ClientTxnKey(branch), value)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &V> {
        self.0.values()
    }

    pub(super) fn shrink_idle(&mut self) {
        shrink_idle(&mut self.0);
    }
}

/// A set of client transaction branches.
#[derive(Default)]
pub(super) struct ClientSet(HashSet<ClientTxnKey>);

impl ClientSet {
    pub(super) fn insert(&mut self, branch: String) -> bool {
        self.0.insert(ClientTxnKey(branch))
    }

    pub(super) fn remove(&mut self, branch: &str) -> bool {
        self.0.remove(lookup(&branch))
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(ClientTxnKey::branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_is_found_whatever_its_case() {
        let mut map = ClientMap::new();
        assert!(map.insert("z9hG4bK-AbC".to_string(), 1).is_none());
        assert_eq!(map.get("Z9HG4BK-abc"), Some(&1));
        assert_eq!(map.insert("z9hg4bk-abc".to_string(), 2), Some(1), "one identity");
        assert_eq!(map.len(), 1);
        assert_eq!(map.remove("z9hG4bK-ABC"), Some(2));
        let mut set = ClientSet::default();
        assert!(set.insert("z9hG4bK-x".to_string()));
        assert!(!set.insert("Z9HG4BK-X".to_string()));
        assert!(set.remove("z9hg4bk-X"));
        assert!(set.is_empty());
    }
}
