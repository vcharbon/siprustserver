//! lookup-map — [`LookupMap`] and [`LookupSet`]: the std hash collections
//! under their random seed, which defends a map keyed by a peer's values,
//! with every way to iterate them left out. Insertion, lookup and removal
//! do not depend on the seed; iteration order is the seed, so a pure crate
//! reads a hash collection through these alone (ADR-0051 §1). Their `Debug`
//! shows the length only.

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

mod map;
mod set;

pub use map::LookupMap;
pub use set::LookupSet;
