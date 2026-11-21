//! The fixed sample of [`cells`] a default lane runs, covering every
//! combination of dimensions a compatible cell holds.

use std::cmp::Reverse;
use std::collections::HashMap;

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
    let mut cover = Cover::of(&legal);
    let first = legal
        .iter()
        .position(|c| *c == CALLEE_EARLY_UPDATES_THEN_REINVITE)
        .expect("the first cell of the sample is compatible");
    let mut picked = vec![first];
    cover.pick(first);
    while cover.uncovered > 0 {
        let best = cover.best();
        cover.pick(best);
        picked.push(best);
    }
    picked.into_iter().map(|i| legal[i]).collect()
}

/// The `(dimension, value index)` coordinates the sample covers together:
/// three of the call's dimensions, or the transfer and one other.
type Combo = Vec<(usize, usize)>;

/// The greedy cover's state over the compatible cells, each named by its index
/// among them, and their combinations, each named by an index of its own.
struct Cover {
    /// Each cell's combinations.
    held: Vec<Vec<usize>>,
    /// Each combination's cells.
    holders: Vec<Vec<usize>>,
    /// Whether a picked cell holds the combination.
    covered: Vec<bool>,
    /// How many of the cell's combinations no picked cell holds.
    gain: Vec<usize>,
    /// How many combinations no picked cell holds.
    uncovered: usize,
}

impl Cover {
    /// The cover of `legal` with no cell picked: every combination uncovered.
    fn of(legal: &[Cell]) -> Self {
        let mut index: HashMap<Combo, usize> = HashMap::new();
        let mut holders: Vec<Vec<usize>> = Vec::new();
        let mut held = Vec::with_capacity(legal.len());
        for (cell, c) in legal.iter().enumerate() {
            let mut of_cell = Vec::new();
            for combo in combos(c) {
                let id = *index.entry(combo).or_insert(holders.len());
                if id == holders.len() {
                    holders.push(Vec::new());
                }
                holders[id].push(cell);
                of_cell.push(id);
            }
            held.push(of_cell);
        }
        Self {
            gain: held.iter().map(Vec::len).collect(),
            covered: vec![false; holders.len()],
            uncovered: holders.len(),
            held,
            holders,
        }
    }

    /// Cover every combination `cell` holds.
    fn pick(&mut self, cell: usize) {
        for &id in &self.held[cell] {
            if !std::mem::replace(&mut self.covered[id], true) {
                self.uncovered -= 1;
                for &holder in &self.holders[id] {
                    self.gain[holder] -= 1;
                }
            }
        }
    }

    /// The cell holding the most uncovered combinations, the earliest among
    /// equals.
    fn best(&self) -> usize {
        (0..self.gain.len())
            .max_by_key(|&cell| (self.gain[cell], Reverse(cell)))
            .expect("some cell is compatible")
    }
}

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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The value tuples `cells` hold on the dimensions `dims`.
    fn values_on(cells: &[Cell], dims: &[usize]) -> HashSet<Vec<usize>> {
        cells
            .iter()
            .map(|c| {
                let co = coordinates(c);
                dims.iter().map(|&d| co[d].1).collect()
            })
            .collect()
    }

    /// Every combination a compatible cell holds (the values of three call
    /// dimensions, or its transfer value with one call dimension's) is held
    /// by a sampled cell.
    #[test]
    fn the_sample_holds_every_combination_a_compatible_cell_holds() {
        let legal: Vec<Cell> = cells().into_iter().filter(|c| compatible(c).is_ok()).collect();
        let sample = sample();
        let call_dims = 0..TRANSFER_DIM;
        let triples = call_dims.clone().flat_map(|i| {
            ((i + 1)..TRANSFER_DIM)
                .flat_map(move |j| ((j + 1)..TRANSFER_DIM).map(move |k| vec![i, j, k]))
        });
        let pairs = call_dims.map(|i| vec![i, TRANSFER_DIM]);
        for dims in triples.chain(pairs) {
            let missing: Vec<_> =
                values_on(&legal, &dims).difference(&values_on(&sample, &dims)).cloned().collect();
            assert!(missing.is_empty(), "dimensions {dims:?}: no sampled cell holds {missing:?}");
        }
    }
}
