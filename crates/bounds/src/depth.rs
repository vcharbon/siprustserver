//! The depth guard of a recursion.

use std::fmt;

/// The stack a descent leaves the running thread, at least: below it the
/// descent is refused, so the frames of one more level and of the error's
/// unwinding fit.
pub const STACK_MARGIN: usize = 64 * 1024;

/// How deep a recursion stands, and how deep it may go. A deeper `Depth` is
/// only made by [`Depth::descend`], so a function that takes one and recurses
/// through `descend` is bounded by its maximum and by the thread's stack.
#[derive(Debug, PartialEq, Eq)]
pub struct Depth {
    level: usize,
    max: usize,
}

/// Why a descent was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooDeep {
    /// The recursion stands at its maximum depth.
    Depth { max: usize },
    /// The thread's stack left is below [`STACK_MARGIN`].
    Stack { left: usize },
}

impl fmt::Display for TooDeep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TooDeep::Depth { max } => write!(f, "nested past {max} levels"),
            TooDeep::Stack { left } => write!(f, "with {left} bytes of stack left"),
        }
    }
}

impl std::error::Error for TooDeep {}

impl Depth {
    /// The top of a recursion that may descend `max` levels.
    pub const fn new(max: usize) -> Depth {
        Depth { level: 0, max }
    }

    /// The levels descended so far.
    pub const fn level(&self) -> usize {
        self.level
    }

    /// One level deeper, or why not: the maximum reached, or the running
    /// thread's stack below [`STACK_MARGIN`] (unknown on a platform `stacker`
    /// cannot read, where the maximum alone holds).
    pub fn descend(&self) -> Result<Depth, TooDeep> {
        if self.level >= self.max {
            return Err(TooDeep::Depth { max: self.max });
        }
        if let Some(left) = stacker::remaining_stack().filter(|&left| left < STACK_MARGIN) {
            return Err(TooDeep::Stack { left });
        }
        Ok(Depth { level: self.level + 1, max: self.max })
    }
}

#[cfg(test)]
mod tests {
    use super::{Depth, TooDeep, STACK_MARGIN};

    /// How deep `depth` lets a recursion go.
    fn reach(depth: &Depth) -> Result<usize, TooDeep> {
        match depth.descend() {
            Ok(deeper) => reach(&deeper),
            Err(TooDeep::Depth { .. }) => Ok(depth.level()),
            Err(stack) => Err(stack),
        }
    }

    #[test]
    fn a_recursion_descends_to_its_maximum_and_no_further() {
        assert_eq!(reach(&Depth::new(64)), Ok(64));
        assert_eq!(Depth::new(0).descend(), Err(TooDeep::Depth { max: 0 }));
    }

    /// A frame of a few KiB, so a small stack runs out within the maximum.
    fn heavy(depth: &Depth) -> Result<usize, TooDeep> {
        let pad = std::hint::black_box([0u8; 4096]);
        let deeper = depth.descend()?;
        Ok(heavy(&deeper)? + usize::from(pad[0]))
    }

    #[test]
    fn a_recursion_on_a_low_stack_is_refused_before_it_overflows() {
        let refused = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| heavy(&Depth::new(usize::MAX)))
            .expect("a thread")
            .join()
            .expect("no overflow");
        assert!(
            matches!(refused, Err(TooDeep::Stack { left }) if left < STACK_MARGIN),
            "{refused:?}"
        );
    }
}
