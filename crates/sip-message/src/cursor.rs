//! The bounded cursor (ADR-0057 §5): a loop over the input runs at most
//! `len + 1` steps, each moving at least one byte on, or ends with
//! [`NoProgress`]. [`walk`] drives a reader written as one step from a
//! position, [`items`] the items such a reader yields one after another.
//! A build for fuzzing panics on a stall, so the fuzzer reports the reader.

use std::fmt;
use std::ops::ControlFlow;

use crate::error::SipParseError;

/// A reader's step that moved no byte on, at byte `at`: a defect of that
/// reader, refused instead of spun on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoProgress {
    /// Where the step started.
    pub at: usize,
}

impl fmt::Display for NoProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a reader made no progress at byte {}", self.at)
    }
}

impl std::error::Error for NoProgress {}

impl From<NoProgress> for SipParseError {
    fn from(stall: NoProgress) -> Self {
        SipParseError::new(stall.to_string())
    }
}

/// The refusal of a step that did not move on from `at`.
pub(crate) fn stalled(at: usize) -> NoProgress {
    #[cfg(fuzzing)]
    panic!("a reader made no progress at byte {at}");
    #[cfg(not(fuzzing))]
    NoProgress { at }
}

/// Runs `step` over `input` from byte `from`: each run reads at the position
/// it is given and either moves on to a later one or ends the walk with its
/// value. A run that does not move on, or a walk still going after
/// `input.len() + 1` runs, ends with [`NoProgress`].
pub(crate) fn walk<T>(
    input: &[u8],
    from: usize,
    mut step: impl FnMut(usize) -> ControlFlow<T, usize>,
) -> Result<T, NoProgress> {
    let mut at = from;
    for _ in 0..=input.len() {
        match step(at) {
            ControlFlow::Break(value) => return Ok(value),
            ControlFlow::Continue(next) if next > at => at = next,
            ControlFlow::Continue(_) => return Err(stalled(at)),
        }
    }
    Err(stalled(at))
}

/// The items `read` takes from `input`, the first at byte 0 and each later
/// one where the last ended: `read` returns an item and the position after
/// it, or `None` where none follows. An item that ends where it started is the
/// last, and so is the `input.len() + 1`-th.
pub(crate) fn items<'a, T: 'a>(
    input: &[u8],
    mut read: impl FnMut(usize) -> Option<(T, usize)> + 'a,
) -> impl Iterator<Item = T> + 'a {
    let len = input.len();
    let mut at = Some(0);
    (0..=len).map_while(move |_| {
        let from = at?;
        let (item, next) = read(from)?;
        at = if next > from {
            Some(next)
        } else {
            let _ = stalled(from);
            None
        };
        Some(item)
    })
}

#[cfg(test)]
mod tests {
    use super::{items, walk, NoProgress};
    use std::ops::ControlFlow;

    #[test]
    fn a_walk_ends_with_the_value_its_step_breaks_with() {
        let input = b"ab;cd";
        let semi = walk(input, 0, |i| match input.get(i) {
            Some(b';') => ControlFlow::Break(Some(i)),
            Some(_) => ControlFlow::Continue(i + 1),
            None => ControlFlow::Break(None),
        });
        assert_eq!(semi, Ok(Some(2)));
    }

    #[test]
    fn a_step_that_does_not_move_on_is_no_progress() {
        let mut runs = 0;
        let stuck: Result<(), _> = walk(b"abc", 1, |i| {
            runs += 1;
            ControlFlow::Continue(i)
        });
        assert_eq!((stuck, runs), (Err(NoProgress { at: 1 }), 1), "the first stalled step ends it");
    }

    #[test]
    fn a_walk_past_its_input_ends_after_len_plus_one_steps() {
        let mut runs = 0;
        let endless: Result<(), _> = walk(b"abc", 0, |i| {
            runs += 1;
            ControlFlow::Continue(i + 1)
        });
        assert_eq!((endless, runs), (Err(NoProgress { at: 4 }), 4));
    }

    #[test]
    fn items_follow_one_another_and_a_stalled_one_is_the_last() {
        let input = b"a,b,c";
        let letters: Vec<u8> = items(input, |i| Some((*input.get(i)?, i + 2))).collect();
        assert_eq!(letters, b"abc");
        let stuck: Vec<u8> = items(input, |i| Some((*input.get(i)?, i))).collect();
        assert_eq!(stuck, b"a");
    }
}
