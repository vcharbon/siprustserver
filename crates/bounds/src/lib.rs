//! bounds — a loop and a recursion bounded by construction (ADR-0057 §5).
//! [`bounded`] runs a loop body at most a maximum number of times and ends
//! with [`Exhausted`] past it; [`Depth`] is the guard a recursion descends
//! through, refused with [`TooDeep`] past its maximum depth or when the
//! running thread's stack falls below [`STACK_MARGIN`]. The panic-sites
//! scanner reads a recursion as guarded when a function on its cycle takes a
//! `Depth` and calls `descend`.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::iter_over_hash_type,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod depth;
mod looped;

pub use depth::{Depth, TooDeep, STACK_MARGIN};
pub use looped::{bounded, Exhausted};
