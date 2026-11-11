//! Which endpoints of a view act as ONE user agent on a Call-ID.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::rules::sides::Sides;
use crate::rules::via::sender_listens_at;
use crate::wire::{Endpoint, Msg};

/// The user agent each endpoint of a view acts as, per Call-ID.
///
/// An endpoint is a user agent of its own until a transaction ties it to
/// another. A response joins its sender to the taker of the request it answers
/// (`Sides`), and its taker to that request's sender when it reached an
/// address that sender wrote (`sender_listens_at`; §18.2.2 answers at the
/// sent-by, not necessarily the source). A tag ties nothing: proxies relay tags
/// unchanged (§16.6). Built over the whole view, since identity is topology,
/// not evidence a party held; each component is named by its smallest endpoint.
#[derive(Debug, Default)]
pub struct Parties<'m> {
    ua: HashMap<&'m str, HashMap<&'m str, &'m str>>,
}

impl<'m> Parties<'m> {
    /// The parties of `msgs`, a view's stream in observation order.
    pub fn of(msgs: impl IntoIterator<Item = &'m Msg>) -> Self {
        let msgs: Vec<&'m Msg> = msgs.into_iter().collect();
        let sides = Sides::of(msgs.iter().copied());
        let mut ua: HashMap<&'m str, HashMap<&'m str, &'m str>> = HashMap::new();
        for msg in msgs.into_iter().filter(|m| !m.repeat) {
            let Some(request) = sides.request_of(msg) else { continue };
            let named = ua.entry(msg.call_id.as_str()).or_default();
            join(named, &msg.src, &request.dst);
            if sender_listens_at(request, &msg.dst) {
                join(named, &msg.dst, &request.src);
            }
        }
        Parties { ua }
    }

    /// The user agent `ep` acts as on `call_id`, named by one of its
    /// endpoints: the key a per-party reading files `ep`'s state under.
    pub fn agent<'s>(&self, call_id: &str, ep: &'s str) -> &'s str
    where
        'm: 's,
    {
        self.ua.get(call_id).and_then(|named| named.get(ep)).copied().unwrap_or(ep)
    }

    /// The endpoints the user agent of `ep` holds on `call_id`, `ep` included.
    pub fn held_on<'s>(&self, call_id: &str, ep: &'s str) -> Vec<&'s str>
    where
        'm: 's,
    {
        let name = self.agent(call_id, ep);
        let named = self.ua.get(call_id).into_iter().flatten();
        let mut held: Vec<&'s str> =
            named.filter(|(_, n)| **n == name).map(|(ep, _)| *ep).collect();
        held.push(ep);
        held
    }

    /// The user agent that sent `msg`, as [`Parties::agent`] names it.
    pub fn sent_by(&self, msg: &'m Msg) -> &'m str {
        self.agent(&msg.call_id, &msg.src)
    }

    /// The user agent that took `msg`, as [`Parties::agent`] names it.
    pub fn took(&self, msg: &'m Msg) -> &'m str {
        self.agent(&msg.call_id, &msg.dst)
    }

    /// Whether `a` and `b` act as one user agent on `call_id`.
    pub fn same(&self, call_id: &str, a: &str, b: &str) -> bool {
        a == b || self.agent(call_id, a) == self.agent(call_id, b)
    }

    /// `(user agent, Call-ID)` of the party that sent `msg`.
    pub fn sender(&self, msg: &Msg) -> (Endpoint, String) {
        (self.agent(&msg.call_id, &msg.src).to_string(), msg.call_id.clone())
    }

    /// `(user agent, Call-ID)` of the party that took `msg`.
    pub fn taker(&self, msg: &Msg) -> (Endpoint, String) {
        (self.agent(&msg.call_id, &msg.dst).to_string(), msg.call_id.clone())
    }

    /// The endpoints one user agent holds on some Call-ID, every two sets that
    /// share an endpoint made one: the sockets of each multi-socket party.
    pub fn joined(&self) -> Vec<BTreeSet<Endpoint>> {
        let mut named: HashMap<&str, &str> = HashMap::new();
        for (ep, name) in self.ua.values().flatten() {
            join(&mut named, ep, name);
        }
        let mut sets: BTreeMap<&str, BTreeSet<Endpoint>> = BTreeMap::new();
        for (ep, name) in named {
            sets.entry(name).or_default().extend([ep.to_string(), name.to_string()]);
        }
        sets.into_values().collect()
    }
}

/// Puts `a` and `b` in one component of `named` (endpoint -> component name; an
/// endpoint absent is its own), named by the smaller name.
fn join<'e>(named: &mut HashMap<&'e str, &'e str>, a: &'e str, b: &'e str) {
    let name =
        |ep: &'e str, named: &HashMap<&'e str, &'e str>| named.get(ep).copied().unwrap_or(ep);
    let (na, nb) = (name(a, named), name(b, named));
    if na == nb {
        return;
    }
    let (keep, gone) = if na < nb { (na, nb) } else { (nb, na) };
    for n in named.values_mut().filter(|n| **n == gone) {
        *n = keep;
    }
    for ep in [a, b, gone] {
        named.insert(ep, keep);
    }
}
