//! The shared machinery of the no-panic property lanes (ADR-0051 §8): the
//! structured mutations every lane applies to its seeds, of SIP wire text
//! ([`mutate`]) and of MessagePack bodies ([`msgpack`]), and the stack, seed
//! and runner every lane runs under ([`lane`]). Each lane keeps its own seeds
//! and its own probe of the entry points it drives.

pub mod lane;
pub mod msgpack;
pub mod mutate;

pub use mutate::Mutation;
