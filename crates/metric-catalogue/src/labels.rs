//! A family's label sets: a union of blocks, each the cartesian product of
//! its label dimensions, enumerated in one fixed order.

/// One label: its name and every value it takes, in exposition order.
#[derive(Debug, Clone, Copy)]
pub struct Dim {
    /// The label name.
    pub name: &'static str,
    /// Every value, in exposition order.
    pub values: &'static [&'static str],
}

impl Dim {
    /// A label named `name` taking `values`, in that order.
    pub const fn new(name: &'static str, values: &'static [&'static str]) -> Self {
        Self { name, values }
    }
}

/// Every label set of a family. Series are enumerated block by block; within
/// a block the last dimension varies fastest, and the labels of a series are
/// written in dimension order.
#[derive(Debug, Clone, Copy)]
pub enum Labels {
    /// One series, without labels.
    None,
    /// The cartesian product of these dimensions.
    Product(&'static [Dim]),
    /// The products of these blocks, block 0 first: a family whose label
    /// sets do not form one product (a label present on some series only).
    Union(&'static [&'static [Dim]]),
}

/// One label set of a family, as the renderer hands it to the value source:
/// which block it belongs to and, per dimension of that block, the index of
/// its value.
#[derive(Debug)]
pub struct Series<'a> {
    block: usize,
    dims: &'static [Dim],
    at: &'a [usize],
}

impl Series<'_> {
    /// The block of [`Labels::Union`] this series belongs to; 0 otherwise.
    pub fn block(&self) -> usize {
        self.block
    }

    /// The index, in `dims[dim].values`, of this series' value of the
    /// dimension at position `dim` of its block. Panics when the block has no
    /// such position.
    pub fn at(&self, dim: usize) -> usize {
        self.at[dim]
    }

    /// The index, in `dim.values`, of this series' value of `dim`, wherever
    /// `dim` sits in the series' block. Panics when the block has no
    /// dimension of that name and values.
    pub fn index(&self, dim: &Dim) -> usize {
        let pos = self.dims.iter().position(|d| d.name == dim.name && d.values == dim.values);
        let pos = pos.unwrap_or_else(|| {
            panic!("block {} has no dimension {:?} {:?}", self.block, dim.name, dim.values)
        });
        self.at[pos]
    }

    /// The `(name, value)` pairs of this series, in dimension order.
    pub fn labels(&self) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
        self.dims.iter().zip(self.at).map(|(d, &i)| (d.name, d.values[i]))
    }
}

impl Labels {
    /// The blocks of label dimensions, in exposition order: one for
    /// [`Labels::None`] (no dimension) and [`Labels::Product`].
    pub fn blocks(&self) -> Vec<&'static [Dim]> {
        match *self {
            Labels::None => vec![&[]],
            Labels::Product(dims) => vec![dims],
            Labels::Union(blocks) => blocks.to_vec(),
        }
    }

    /// The block whose label names are `names`, in that order.
    pub(crate) fn block_named<S: AsRef<str>>(&self, names: &[S]) -> Option<&'static [Dim]> {
        self.blocks().into_iter().find(|dims| {
            dims.len() == names.len() && dims.iter().zip(names).all(|(d, n)| d.name == n.as_ref())
        })
    }

    /// The first block of `arity` dimensions: the one a semi-open family's
    /// label set of that many values is named by. Panics when no block has
    /// that many dimensions, or two such blocks name them differently.
    pub(crate) fn block_of_arity(&self, arity: usize) -> &'static [Dim] {
        let mut found = self.blocks().into_iter().filter(|dims| dims.len() == arity);
        let block = found.next().unwrap_or_else(|| panic!("no block of {arity} labels"));
        for other in found {
            let same = block.iter().zip(other).all(|(a, b)| a.name == b.name);
            assert!(same, "two blocks of {arity} labels name them differently");
        }
        block
    }

    /// How many series the family has.
    pub(crate) fn series_count(&self) -> usize {
        self.blocks()
            .iter()
            .map(|dims| dims.iter().map(|d| d.values.len()).product::<usize>())
            .sum()
    }

    /// Visit every series, in exposition order.
    pub fn for_each_series(&self, mut f: impl FnMut(&Series<'_>)) {
        for (block, dims) in self.blocks().into_iter().enumerate() {
            if dims.iter().any(|d| d.values.is_empty()) {
                continue;
            }
            let mut at = vec![0usize; dims.len()];
            loop {
                f(&Series { block, dims, at: &at });
                // Odometer step: the last dimension varies fastest.
                let mut d = dims.len();
                loop {
                    if d == 0 {
                        break;
                    }
                    d -= 1;
                    at[d] += 1;
                    if at[d] < dims[d].values.len() {
                        break;
                    }
                    at[d] = 0;
                }
                if at.iter().all(|&i| i == 0) {
                    break;
                }
            }
        }
    }

    /// Every label set as `(name, value)` pairs, in exposition order.
    pub(crate) fn label_sets(&self) -> Vec<Vec<(&'static str, &'static str)>> {
        let mut out = Vec::with_capacity(self.series_count());
        self.for_each_series(|s| out.push(s.labels().collect()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS: Dim = Dim::new("class", &["normal", "emergency"]);
    const BOUND: Dim = Dim::new("bound", &["calls", "transactions", "rss"]);

    #[test]
    fn no_labels_is_one_series_without_labels() {
        assert_eq!(Labels::None.series_count(), 1);
        assert_eq!(Labels::None.label_sets(), vec![Vec::new()]);
    }

    #[test]
    fn a_product_varies_its_last_dimension_fastest() {
        let labels = Labels::Product(&[BOUND, CLASS]);
        assert_eq!(labels.series_count(), 6);
        let sets = labels.label_sets();
        assert_eq!(sets[0], vec![("bound", "calls"), ("class", "normal")]);
        assert_eq!(sets[1], vec![("bound", "calls"), ("class", "emergency")]);
        assert_eq!(sets[2], vec![("bound", "transactions"), ("class", "normal")]);
        assert_eq!(sets[5], vec![("bound", "rss"), ("class", "emergency")]);
    }

    #[test]
    fn a_union_enumerates_its_blocks_in_order_with_their_indices() {
        const ACCEPTED: Dim = Dim::new("outcome", &["accepted"]);
        const REJECTED: Dim = Dim::new("outcome", &["rejected"]);
        const REASON: Dim = Dim::new("reason", &["a", "b"]);
        let labels = Labels::Union(&[&[ACCEPTED, CLASS], &[REJECTED, REASON, CLASS]]);
        assert_eq!(labels.series_count(), 2 + 4);
        let mut seen = Vec::new();
        labels.for_each_series(|s| {
            let idx: Vec<usize> = (0..s.labels().count()).map(|d| s.at(d)).collect();
            seen.push((s.block(), idx));
        });
        assert_eq!(
            seen,
            vec![
                (0, vec![0, 0]),
                (0, vec![0, 1]),
                (1, vec![0, 0, 0]),
                (1, vec![0, 0, 1]),
                (1, vec![0, 1, 0]),
                (1, vec![0, 1, 1]),
            ]
        );
    }

    #[test]
    fn a_dimension_is_found_by_name_and_values_wherever_it_sits() {
        const ACCEPTED: Dim = Dim::new("outcome", &["accepted"]);
        const REJECTED: Dim = Dim::new("outcome", &["rejected"]);
        const REASON: Dim = Dim::new("reason", &["a", "b"]);
        let labels = Labels::Union(&[&[ACCEPTED, CLASS], &[REJECTED, REASON, CLASS]]);
        let mut seen = Vec::new();
        labels.for_each_series(|s| {
            let reason = (s.block() == 1).then(|| s.index(&REASON));
            seen.push((s.block(), reason, s.index(&CLASS)));
        });
        assert_eq!(
            seen,
            vec![
                (0, None, 0),
                (0, None, 1),
                (1, Some(0), 0),
                (1, Some(0), 1),
                (1, Some(1), 0),
                (1, Some(1), 1),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "has no dimension")]
    fn a_dimension_absent_from_the_block_panics() {
        const REASON: Dim = Dim::new("reason", &["a"]);
        Labels::Product(&[CLASS]).for_each_series(|s| {
            s.index(&REASON);
        });
    }

    #[test]
    #[should_panic(expected = "has no dimension")]
    fn a_dimension_of_the_same_name_but_other_values_is_not_found() {
        const OUTCOME: Dim = Dim::new("outcome", &["accepted"]);
        const OTHER: Dim = Dim::new("outcome", &["rejected"]);
        Labels::Product(&[OUTCOME]).for_each_series(|s| {
            s.index(&OTHER);
        });
    }

    #[test]
    fn a_dimension_without_values_contributes_no_series() {
        const EMPTY: Dim = Dim::new("x", &[]);
        let labels = Labels::Union(&[&[EMPTY, CLASS], &[CLASS]]);
        assert_eq!(labels.series_count(), 2);
        assert_eq!(labels.label_sets().len(), 2);
    }
}
