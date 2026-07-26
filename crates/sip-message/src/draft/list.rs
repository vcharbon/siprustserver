//! [`HeaderList`] — the one logical view of a multi-valued header.
//!
//! Wire lines and comma folds flatten into a single ordered list of values, so
//! the RFC 3261 §7.3.1 aware "pop the top entry" lives here once instead of at
//! every consumer that has to remove a Via or a Route.

use crate::header::HeaderValue;

/// Every value of one header, in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderList<H: HeaderValue> {
    values: Vec<H>,
}

impl<H: HeaderValue> HeaderList<H> {
    pub fn new(values: Vec<H>) -> Self {
        Self { values }
    }

    pub fn empty() -> Self {
        Self { values: Vec::new() }
    }

    /// Add a value at the top — where a proxy's own Via and Route go.
    pub fn push_front(mut self, value: H) -> Self {
        self.values.insert(0, value);
        self
    }

    pub fn push_back(mut self, value: H) -> Self {
        self.values.push(value);
        self
    }

    /// Take the top value off.
    pub fn pop_front(&mut self) -> Option<H> {
        if self.values.is_empty() {
            None
        } else {
            Some(self.values.remove(0))
        }
    }

    pub fn first(&self) -> Option<&H> {
        self.values.first()
    }

    pub fn last(&self) -> Option<&H> {
        self.values.last()
    }

    pub fn iter(&self) -> impl Iterator<Item = &H> {
        self.values.iter()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Rewrite the top value — the shape of stamping `received`/`rport`.
    pub fn map_first(mut self, f: impl FnOnce(H) -> H) -> Self {
        if !self.values.is_empty() {
            let head = self.values.remove(0);
            self.values.insert(0, f(head));
        }
        self
    }

    pub fn map(mut self, f: impl FnMut(H) -> H) -> Self {
        self.values = self.values.into_iter().map(f).collect();
        self
    }

    pub fn retain(mut self, f: impl FnMut(&H) -> bool) -> Self {
        self.values.retain(f);
        self
    }

    /// The list in reverse — a Record-Route set read as the route set to apply
    /// (RFC 3261 §12.1.1).
    pub fn reversed(mut self) -> Self {
        self.values.reverse();
        self
    }

    pub fn into_vec(self) -> Vec<H> {
        self.values
    }
}

impl<H: HeaderValue> FromIterator<H> for HeaderList<H> {
    fn from_iter<I: IntoIterator<Item = H>>(iter: I) -> Self {
        Self { values: iter.into_iter().collect() }
    }
}

impl<H: HeaderValue> IntoIterator for HeaderList<H> {
    type Item = H;
    type IntoIter = std::vec::IntoIter<H>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::Via;

    fn hops() -> HeaderList<Via> {
        HeaderList::new(vec![Via::udp("a", 5060), Via::udp("b", 5060), Via::udp("c", 5060)])
    }

    #[test]
    fn the_top_hop_is_pushed_and_popped_at_the_front() {
        let list = hops().push_front(Via::udp("proxy", 5080));
        assert_eq!(list.first().unwrap().host(), "proxy");
        let mut list = list;
        assert_eq!(list.pop_front().unwrap().host(), "proxy");
        assert_eq!(list.first().unwrap().host(), "a");
    }

    #[test]
    fn the_top_hop_is_rewritten_in_place() {
        let list = hops().map_first(|v| v.with_received("192.0.2.9"));
        assert_eq!(list.first().unwrap().received(), Some("192.0.2.9"));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn reversing_yields_the_route_set_order() {
        let reversed = hops().reversed().into_vec();
        let hosts: Vec<&str> = reversed.iter().map(|v| v.host()).collect();
        assert_eq!(hosts, ["c", "b", "a"]);
    }
}
