//! The shared machinery of the no-panic property lanes (ADR-0051 §8): the
//! structured mutations every lane applies to its synthetic seeds
//! ([`mutate`]), and the stack, seed and runner every lane runs under
//! ([`lane`]). Each lane keeps its own seeds and its own probe of the entry
//! points it drives.

pub mod lane;
pub mod mutate;

pub use mutate::Mutation;
