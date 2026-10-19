//! metric-catalogue — a process's Prometheus metric families declared as
//! data, so a family's name, kind, help and label sets are stated once and
//! every renderer and test reads them from there.
//!
//! - [`Family`] — one catalogue entry: name, [`Kind`], [`Labels`],
//!   [`Presence`], help.
//! - [`Labels`] / [`Dim`] — a family's label sets, a union of cartesian
//!   products of label dimensions; [`label_values!`] builds a dimension's
//!   values from an enum's exposition order, and [`assert_exposition_order!`]
//!   proves at compile time that the order lists every variant.
//! - [`Family::render`] — the text exposition of a family from its entry and
//!   one value per [`Series`]; every label set is written.
//! - [`Family::check`] — whether a scraped text holds the family exactly as
//!   declared (HELP, TYPE, every label set once, in order, grouped).
//! - [`Presence`] — a family is fixed (its declared label sets, at 0 before
//!   their first event) or semi-open (those, then any label set nobody
//!   declared from its first observation, under a [`Cap`] where the values
//!   are unbounded).
//! - [`Catalogue`] — the families of one binary's `/metrics` body, checked
//!   whole; [`export::to_json`] states every catalogue for readers.
//!
//! Pure: no clock, no I/O, no global registry, no shared state. A process
//! holds its values (the `metric-values` crate) and hands them to the
//! renderer at scrape time.
//!
//! A declaration a caller uses against its kind or its label sets is a
//! caller's bug, refused by a debug assertion, which every binary's catalogue
//! test runs over its static declarations. A release build goes on: a render
//! of the other kind writes nothing, a series position or dimension the block
//! lacks reads index 0, a row of an arity no block declares is written with no
//! labels, and an undeclared row of a fixed family is not written.

#![forbid(unsafe_code)]
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

mod catalogue;
mod check;
pub mod export;
mod family;
mod labels;
mod render;

pub use catalogue::Catalogue;
pub use check::Mismatch;
pub use family::{Cap, Family, Kind, Presence, DEFAULT_CAP, OVERFLOW};
pub use labels::{Dim, Labels, Series};
pub use render::HistogramValue;

/// The values of a label dimension, from an array of `Copy` enum variants in
/// exposition order and a `const fn` naming each one: usable in a `const`.
///
/// ```
/// #[derive(Clone, Copy)]
/// enum Class { Normal, Emergency }
/// impl Class {
///     const ALL: [Class; 2] = [Class::Normal, Class::Emergency];
///     const fn label(self) -> &'static str {
///         match self { Class::Normal => "normal", Class::Emergency => "emergency" }
///     }
/// }
/// const CLASS_VALUES: [&str; 2] = metric_catalogue::label_values!(Class::ALL, Class::label);
/// assert_eq!(CLASS_VALUES, ["normal", "emergency"]);
/// ```
#[macro_export]
macro_rules! label_values {
    ($all:expr, $label:expr) => {{
        let all = $all;
        let mut out = [""; $all.len()];
        let mut i = 0;
        while i < out.len() {
            out[i] = $label(all[i]);
            i += 1;
        }
        out
    }};
}

/// A compile-time proof that a fieldless enum's `ALL` array lists every
/// variant once, in declaration order: `ALL[i] as usize == i`, and `ALL` is
/// as long as the variant list given here, which a `match` with no wildcard
/// keeps exhaustive. A variant added to the enum fails to compile until it
/// is listed here, and then until `ALL` holds it in its place.
///
/// ```
/// #[derive(Clone, Copy)]
/// enum Class { Normal, Emergency }
/// impl Class {
///     const ALL: [Class; 2] = [Class::Normal, Class::Emergency];
/// }
/// metric_catalogue::assert_exposition_order!(Class: Normal, Emergency);
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy)]
/// enum Class { Normal, Emergency, InDialog }
/// impl Class {
///     const ALL: [Class; 2] = [Class::Normal, Class::Emergency];
/// }
/// metric_catalogue::assert_exposition_order!(Class: Normal, Emergency);
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy)]
/// enum Class { Normal, Emergency }
/// impl Class {
///     const ALL: [Class; 2] = [Class::Emergency, Class::Normal];
/// }
/// metric_catalogue::assert_exposition_order!(Class: Normal, Emergency);
/// ```
#[macro_export]
macro_rules! assert_exposition_order {
    ($ty:ident : $($variant:ident),+ $(,)?) => {
        const _: () = {
            let listed = [$($ty::$variant),+];
            match listed[0] {
                $($ty::$variant)|+ => {}
            }
            #[expect(clippy::disallowed_macros, reason = "evaluated at compile time")]
            assert!($ty::ALL.len() == listed.len(), "ALL does not list every variant");
            let mut i = 0;
            while i < listed.len() {
                #[expect(clippy::disallowed_macros, reason = "evaluated at compile time")]
                assert!($ty::ALL[i] as usize == i, "ALL is not in declaration order");
                #[expect(clippy::disallowed_macros, reason = "evaluated at compile time")]
                assert!(listed[i] as usize == i, "the listed variants are not in declaration order");
                i += 1;
            }
        };
    };
}
