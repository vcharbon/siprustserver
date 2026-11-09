//! A loop charged one unit per iteration.

use std::fmt;
use std::ops::ControlFlow;

/// A loop ran its maximum number of iterations without ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted {
    /// The iterations it was allowed.
    pub max: usize,
}

impl fmt::Display for Exhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a loop ran {} iterations without ending", self.max)
    }
}

impl std::error::Error for Exhausted {}

/// Runs `step` until it breaks, at most `max` times: the value it breaks
/// with, or [`Exhausted`] when the `max`-th run still continues.
pub fn bounded<T>(max: usize, mut step: impl FnMut() -> ControlFlow<T>) -> Result<T, Exhausted> {
    for _ in 0..max {
        if let ControlFlow::Break(value) = step() {
            return Ok(value);
        }
    }
    Err(Exhausted { max })
}

#[cfg(test)]
mod tests {
    use super::{bounded, Exhausted};
    use std::ops::ControlFlow;

    /// Sums a stack of three: four runs, the last finding it empty.
    fn sum_within(max: usize) -> Result<i32, Exhausted> {
        let (mut left, mut sum) = (vec![1, 2, 3], 0);
        bounded(max, || match left.pop() {
            Some(n) => {
                sum += n;
                ControlFlow::Continue(())
            }
            None => ControlFlow::Break(sum),
        })
    }

    #[test]
    fn a_loop_ends_with_its_value_at_its_maximum_and_is_exhausted_below() {
        assert_eq!(sum_within(4), Ok(6));
        assert_eq!(sum_within(3), Err(Exhausted { max: 3 }));
    }

    #[test]
    fn a_loop_that_never_breaks_ends_at_its_maximum() {
        let mut runs = 0;
        let got: Result<(), _> = bounded(5, || {
            runs += 1;
            ControlFlow::Continue(())
        });
        assert_eq!((got, runs), (Err(Exhausted { max: 5 }), 5));
    }
}
