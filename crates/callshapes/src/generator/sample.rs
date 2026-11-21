//! The fixed sample of [`cells`] a default lane runs, covering every
//! combination of dimensions a compatible cell holds.

use std::collections::HashSet;

use super::{
    cells, compatible, Cell, AFTERS, CALLEE_EARLY_UPDATES_THEN_REINVITE, EARLIES, ENDINGS,
    RELIABLES, SENDERS, TALKS, TRANSFERS, WINNERS,
};

/// The fixed sample a default lane runs: the [`CALLEE_EARLY_UPDATES_THEN_REINVITE`]
/// cell first, then compatible cells picked greedily (the earliest in
/// [`cells`] order among the best) until every combination (`combos`) some
/// compatible cell holds is held by a picked cell: every triple of the call's
/// dimensions, and every transfer value with every other dimension's value.
pub fn sample() -> Vec<Cell> {
    let legal: Vec<Cell> = cells().into_iter().filter(|c| compatible(c).is_ok()).collect();
    let mut uncovered: HashSet<Combo> = legal.iter().flat_map(combos).collect();
    let mut picked = Vec::new();
    let mut pick = |cell: Cell, uncovered: &mut HashSet<Combo>| {
        for t in combos(&cell) {
            uncovered.remove(&t);
        }
        picked.push(cell);
    };
    pick(CALLEE_EARLY_UPDATES_THEN_REINVITE, &mut uncovered);
    while !uncovered.is_empty() {
        let gain = |c: &Cell| combos(c).filter(|t| uncovered.contains(t)).count();
        let (_, best) = legal
            .iter()
            .enumerate()
            .map(|(i, c)| ((gain(c), std::cmp::Reverse(i)), *c))
            .max_by_key(|(key, _)| *key)
            .expect("a compatible cell holds each uncovered combination");
        pick(best, &mut uncovered);
    }
    picked
}

/// The `(dimension, value index)` coordinates the sample covers together:
/// three of the call's dimensions, or the transfer and one other.
type Combo = Vec<(usize, usize)>;

/// The dimension index of [`Cell::transfer`], covered pairwise only: a
/// transfer's interplay with each other dimension, at a sample size the
/// default lane can run.
const TRANSFER_DIM: usize = 7;

/// Each dimension's value as `(dimension, value index)`.
pub(super) fn coordinates(c: &Cell) -> [(usize, usize); 8] {
    let at = |i: Option<usize>| i.expect("every value is listed");
    [
        (0, at(SENDERS.iter().position(|v| *v == c.sender))),
        (1, at(EARLIES.iter().position(|v| *v == c.early))),
        (2, at(AFTERS.iter().position(|v| *v == c.after))),
        (3, at(WINNERS.iter().position(|v| *v == c.winner))),
        (4, at(RELIABLES.iter().position(|v| *v == c.reliable))),
        (5, at(ENDINGS.iter().position(|v| *v == c.ending))),
        (6, at(TALKS.iter().position(|v| *v == c.talk))),
        (TRANSFER_DIM, at(TRANSFERS.iter().position(|v| *v == c.transfer))),
    ]
}

/// The combinations `c` holds: every triple of its call dimensions, and its
/// transfer value with each call dimension's.
fn combos(c: &Cell) -> impl Iterator<Item = Combo> {
    let co = coordinates(c);
    let n = TRANSFER_DIM;
    let triples = (0..n).flat_map(move |i| {
        ((i + 1)..n).flat_map(move |j| ((j + 1)..n).map(move |k| vec![co[i], co[j], co[k]]))
    });
    let pairs = (0..n).map(move |i| vec![co[i], co[TRANSFER_DIM]]);
    triples.chain(pairs)
}
