//! Which endpoints of a view act as ONE user agent on a Call-ID.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::rules::branch::BranchName;
use crate::rules::via::sender_listens_at;
use crate::wire::{Endpoint, Kind, Msg};

/// The user agent each endpoint of a view acts as, per Call-ID.
///
/// An endpoint is a user agent of its own until a transaction ties it to
/// another. A response joins its sender to the taker of the request it answers,
/// and its taker to that request's sender when it reached an address that
/// sender wrote (`sender_listens_at`; §18.2.2 answers at the sent-by, not
/// necessarily the source). The request is the one of the same Call-ID, top-Via
/// branch and CSeq method (§17.1.3); a key carried between two different
/// endpoint pairs ties nothing. A tag ties nothing: proxies relay tags
/// unchanged (§16.6). Built over the whole view, since identity is topology,
/// not evidence a party held; each component is named by its smallest endpoint.
pub struct Parties {
    ua: HashMap<String, HashMap<Endpoint, Endpoint>>,
}

/// The first request one `(Call-ID, branch, method)` names: `None` once a
/// request between another endpoint pair carried it.
type Answered<'m> = Option<&'m Msg>;

impl Parties {
    /// The parties of `msgs`, a view's stream in observation order.
    pub fn of<'m>(msgs: impl IntoIterator<Item = &'m Msg>) -> Self {
        let mut requests: BTreeMap<TransactionKey<'m>, Answered<'m>> = BTreeMap::new();
        let mut responses: Vec<(TransactionKey<'m>, &Msg)> = Vec::new();
        for msg in msgs.into_iter().filter(|m| !m.repeat && !m.call_id.is_empty()) {
            let Some(key) = transaction_key(msg) else { continue };
            match msg.kind {
                Kind::Request { .. } => {
                    requests
                        .entry(key)
                        .and_modify(|seen| {
                            if seen.is_some_and(|r| (&r.src, &r.dst) != (&msg.src, &msg.dst)) {
                                *seen = None;
                            }
                        })
                        .or_insert(Some(msg));
                }
                Kind::Response { .. } => responses.push((key, msg)),
            }
        }
        let mut ua: HashMap<String, HashMap<Endpoint, Endpoint>> = HashMap::new();
        for (key, msg) in responses {
            if let Some(Some(request)) = requests.get(&key) {
                let named = ua.entry(msg.call_id.clone()).or_default();
                join(named, msg.src.clone(), request.dst.clone());
                if sender_listens_at(request, &msg.dst) {
                    join(named, msg.dst.clone(), request.src.clone());
                }
            }
        }
        Parties { ua }
    }

    /// The user agent `ep` acts as on `call_id`.
    fn of_endpoint(&self, ep: &Endpoint, call_id: &str) -> Endpoint {
        self.ua.get(call_id).and_then(|named| named.get(ep)).unwrap_or(ep).clone()
    }

    /// `(user agent, Call-ID)` of the party that sent `msg`.
    pub fn sender(&self, msg: &Msg) -> (Endpoint, String) {
        (self.of_endpoint(&msg.src, &msg.call_id), msg.call_id.clone())
    }

    /// `(user agent, Call-ID)` of the party that took `msg`.
    pub fn taker(&self, msg: &Msg) -> (Endpoint, String) {
        (self.of_endpoint(&msg.dst, &msg.call_id), msg.call_id.clone())
    }

    /// The endpoints one user agent holds on some Call-ID, every two sets that
    /// share an endpoint made one: the sockets of each multi-socket party.
    pub fn joined(&self) -> Vec<BTreeSet<Endpoint>> {
        let mut named: HashMap<Endpoint, Endpoint> = HashMap::new();
        for (ep, name) in self.ua.values().flatten() {
            join(&mut named, ep.clone(), name.clone());
        }
        let mut sets: BTreeMap<Endpoint, BTreeSet<Endpoint>> = BTreeMap::new();
        for (ep, name) in named {
            sets.entry(name.clone()).or_default().extend([ep, name]);
        }
        sets.into_values().collect()
    }
}

/// Puts `a` and `b` in one component of `named` (endpoint -> component name; an
/// endpoint absent is its own), named by the smaller name.
fn join(named: &mut HashMap<Endpoint, Endpoint>, a: Endpoint, b: Endpoint) {
    let name =
        |ep: &Endpoint, named: &HashMap<Endpoint, Endpoint>| named.get(ep).unwrap_or(ep).clone();
    let (na, nb) = (name(&a, named), name(&b, named));
    if na == nb {
        return;
    }
    let (keep, gone) = if na < nb { (na, nb) } else { (nb, na) };
    for n in named.values_mut().filter(|n| **n == gone) {
        n.clone_from(&keep);
    }
    for ep in [a, b, gone] {
        named.insert(ep, keep.clone());
    }
}

/// `(Call-ID, top-Via branch, CSeq method)`: the transaction a message belongs
/// to (§17.1.3).
type TransactionKey<'m> = (&'m str, BranchName<'m>, String);

/// The transaction `msg` belongs to, `None` without a branch.
fn transaction_key(msg: &Msg) -> Option<TransactionKey<'_>> {
    let branch = BranchName(msg.via_branch.as_deref()?);
    Some((msg.call_id.as_str(), branch, msg.cseq_method.to_ascii_uppercase()))
}
