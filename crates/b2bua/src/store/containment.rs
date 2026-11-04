//! Two copies of one call incarnation meeting after a split (ADR-0055 §7):
//! which one contains the other, read from the state stamp and the version
//! vector alone.
//!
//! Containment holds for two copies of one call incarnation, under pull-only
//! replication, and with no vector ever taken without the body that carried
//! it: a copy's vector at or past a write's means it read that write's body.
//! R contains L when L's stamp — the vector of L's last write that changed a
//! compared field (`call::helpers::same_state`) — is at or below R's vector,
//! component by component: R read every compared write L made.

use super::envelope::{CallTopology, Enveloped};

/// How two copies of one call incarnation stand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Containment {
    /// Each contains the other: neither made a compared write the other did
    /// not read.
    Both,
    /// Only the held copy contains the incoming one.
    Held,
    /// Only the incoming copy contains the held one.
    Incoming,
    /// Each made a compared write the other never read: the copies diverged.
    Neither,
}

/// Whether the copy at `r` contains the copy at `l`: `l`'s stamp is at or
/// below `r`'s vector, component by component.
pub fn contains(r: &CallTopology, l: &CallTopology) -> bool {
    let ((sp, sb), (vp, vb)) = (l.stamp(), r.vector());
    sp <= vp && sb <= vb
}

/// How the copy at `held` and the copy at `incoming` stand.
pub fn containment(held: &CallTopology, incoming: &CallTopology) -> Containment {
    match (contains(held, incoming), contains(incoming, held)) {
        (true, true) => Containment::Both,
        (true, false) => Containment::Held,
        (false, true) => Containment::Incoming,
        (false, false) => Containment::Neither,
    }
}

/// What a meeting of two copies keeps.
#[derive(Clone, Debug, PartialEq)]
pub enum Met {
    /// The held copy, whole, the incoming copy's exempt fields merged in.
    Held(Enveloped),
    /// The incoming copy, whole, the held copy's exempt fields merged in.
    Incoming(Enveloped),
    /// Neither contains the other: the call ends cleanly from the union of
    /// both (`router::diverged`).
    Diverged,
}

/// `held` meets `incoming`, two copies of one call incarnation: the copy that
/// contains the other is taken whole with the other's exempt fields merged
/// in ([`Enveloped::merge_exempt`]: the vector at the component-wise
/// maximum, the stamp the container's); when each contains the other, the
/// primary's — `incoming` when `incoming_is_primary`. A copy with no topology
/// is not replicated and meets nothing: `held` stands.
pub fn meet(held: Enveloped, incoming: &Enveloped, incoming_is_primary: bool) -> Met {
    let (Some(h), Some(i)) = (held.topology.as_ref(), incoming.topology.as_ref()) else {
        return Met::Held(held);
    };
    match containment(h, i) {
        Containment::Held => Met::Held(held.merge_exempt(incoming)),
        Containment::Incoming => Met::Incoming(incoming.clone().merge_exempt(&held)),
        Containment::Both if incoming_is_primary => {
            Met::Incoming(incoming.clone().merge_exempt(&held))
        }
        Containment::Both => Met::Held(held.merge_exempt(incoming)),
        Containment::Neither => Met::Diverged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(gen: i64, bak_gen: i64, stamp: (i64, i64)) -> CallTopology {
        let mut t = CallTopology::new("w0", "w1", gen, bak_gen);
        (t.stamp_gen, t.stamp_bak_gen) = stamp;
        t
    }

    /// A copy that read the other's last compared write contains it, however
    /// many exempt writes the other made since.
    #[test]
    fn a_copy_contains_the_one_whose_stamp_it_read() {
        let primary = at(6, 2, (6, 2));
        let copy = at(5, 4, (5, 2));
        assert!(contains(&primary, &copy), "the copy's exempt writes are no state");
        assert!(!contains(&copy, &primary), "the primary's last write is unread");
        assert_eq!(containment(&primary, &copy), Containment::Held);
        assert_eq!(containment(&copy, &primary), Containment::Incoming);
    }

    /// Two copies that each made a compared write the other never read
    /// diverged.
    #[test]
    fn two_unread_compared_writes_diverge() {
        assert_eq!(containment(&at(6, 2, (6, 2)), &at(5, 3, (5, 3))), Containment::Neither);
    }

    /// Two copies whose writes since their common version changed only exempt
    /// fields contain each other.
    #[test]
    fn exempt_writes_on_both_sides_contain_each_other() {
        assert_eq!(containment(&at(6, 2, (5, 2)), &at(5, 3, (5, 2))), Containment::Both);
    }
}
