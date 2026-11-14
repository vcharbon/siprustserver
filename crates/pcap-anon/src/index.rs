//! Indices over one capture, built once per run and read by every per-leg
//! question: the call groups of a leg and the Call-ID suffix pairs. A lookup
//! costs the size of its answer, never the size of the capture.

use std::collections::HashMap;

use crate::loopback;
use crate::model::Flows;

/// The plan-free structure of a capture: groups and derivations.
#[derive(Debug, Default)]
pub struct Derivations {
    groups_of: Vec<Vec<usize>>,
    derived_of: Vec<Vec<usize>>,
    bases_of: Vec<Vec<usize>>,
}

impl Derivations {
    /// Index `flows`: one pass over the groups and one probe per proper
    /// suffix of every Call-ID.
    pub fn build(flows: &Flows) -> Self {
        let n = flows.legs.len();
        let mut groups_of = vec![Vec::new(); n];
        for (gi, g) in flows.groups.iter().enumerate() {
            for &l in &g.legs {
                if l < n && groups_of[l].last() != Some(&gi) {
                    groups_of[l].push(gi);
                }
            }
        }
        let mut by_call_id: HashMap<&str, Vec<usize>> = HashMap::new();
        for (li, leg) in flows.legs.iter().enumerate() {
            by_call_id.entry(leg.call_id.as_str()).or_default().push(li);
        }
        let mut derived_of = vec![Vec::new(); n];
        let mut bases_of = vec![Vec::new(); n];
        for (di, leg) in flows.legs.iter().enumerate() {
            let cid = leg.call_id.as_str();
            for (start, _) in cid.char_indices().skip(1) {
                if let Some(bases) = by_call_id.get(&cid[start..]) {
                    for &bi in bases {
                        derived_of[bi].push(di);
                        bases_of[di].push(bi);
                    }
                }
            }
        }
        for v in derived_of.iter_mut().chain(bases_of.iter_mut()) {
            v.sort_unstable();
        }
        Self { groups_of, derived_of, bases_of }
    }

    /// The groups `leg` belongs to, ascending.
    pub fn groups_of(&self, leg: usize) -> &[usize] {
        self.groups_of.get(leg).map_or(&[], Vec::as_slice)
    }

    /// Whether some group holds both legs.
    pub fn same_group(&self, a: usize, b: usize) -> bool {
        let (ga, gb) = (self.groups_of(a), self.groups_of(b));
        ga.iter().any(|g| gb.binary_search(g).is_ok())
    }

    /// The legs whose Call-ID is a non-empty prefix ahead of `base`'s, ascending
    /// — the suffix relation alone, before any minimum base length. A base is
    /// a non-empty Call-ID: an empty one is a suffix of nothing here.
    pub fn suffixed_by(&self, base: usize) -> &[usize] {
        self.derived_of.get(base).map_or(&[], Vec::as_slice)
    }

    /// The legs whose Call-ID is a proper suffix of `derived`'s, ascending.
    pub fn suffixes_of(&self, derived: usize) -> &[usize] {
        self.bases_of.get(derived).map_or(&[], Vec::as_slice)
    }

    /// The legs an application server minted from `base` (`loopback::derives`).
    pub fn derived_of<'a>(
        &'a self,
        flows: &'a Flows,
        base: usize,
    ) -> impl Iterator<Item = usize> + 'a {
        let base_cid = &flows.legs[base].call_id;
        self.suffixed_by(base)
            .iter()
            .copied()
            .filter(move |&d| loopback::derives(base_cid, &flows.legs[d].call_id))
    }

    /// The legs `derived` was minted from (`loopback::derives`).
    pub fn bases_of<'a>(
        &'a self,
        flows: &'a Flows,
        derived: usize,
    ) -> impl Iterator<Item = usize> + 'a {
        let derived_cid = &flows.legs[derived].call_id;
        self.suffixes_of(derived)
            .iter()
            .copied()
            .filter(move |&b| loopback::derives(&flows.legs[b].call_id, derived_cid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testflow::{flows, leg, peer_req};

    #[test]
    fn derivations_index_groups_and_suffixes() {
        let ladder = |ms: u64| [peer_req(ms, "INVITE")];
        let f = flows(
            &[
                leg("abcdefgh@host", &ladder(1)),
                leg("2-abcdefgh@host", &ladder(2)),
                leg("3-abcdefgh@host", &ladder(3)),
                leg("host", &ladder(4)),
                leg("x-host", &ladder(5)),
            ],
            &[&[0, 1], &[2], &[3], &[4]],
        );
        let d = Derivations::build(&f);
        assert_eq!(d.groups_of(1), &[0]);
        assert!(d.same_group(0, 1) && !d.same_group(0, 2));
        assert_eq!(d.suffixes_of(1), &[0, 3]);
        assert_eq!(d.suffixed_by(3), &[0, 1, 2, 4]);
        assert_eq!(d.suffixed_by(0), &[1, 2]);
        // The derivation relation needs a base of at least `MIN_BASE_LEN`.
        assert_eq!(d.bases_of(&f, 1).collect::<Vec<_>>(), vec![0]);
        assert_eq!(d.derived_of(&f, 3).count(), 0);
        assert_eq!(d.derived_of(&f, 0).collect::<Vec<_>>(), vec![1, 2]);
        // One predicate: the index's derivation pairs are `derives` over all pairs.
        for b in 0..f.legs.len() {
            let want: Vec<usize> = (0..f.legs.len())
                .filter(|&x| loopback::derives(&f.legs[b].call_id, &f.legs[x].call_id))
                .collect();
            assert_eq!(d.derived_of(&f, b).collect::<Vec<_>>(), want, "base {b}");
        }
    }
}
