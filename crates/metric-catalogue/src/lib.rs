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
//!   are unbounded); [`OpenRows`] holds a semi-open family's rows,
//!   [`FixedCounts`] a fixed family's counts keyed by label values.
//! - [`Catalogue`] — the families of one binary's `/metrics` body, checked
//!   whole; [`export::to_json`] states every catalogue for readers.
//!
//! Pure: no clock, no I/O, no global registry. A process owns its values and
//! hands them to the renderer at scrape time.

#![forbid(unsafe_code)]

mod catalogue;
mod check;
pub mod export;
mod family;
mod fixed;
mod labels;
mod open;
mod render;

pub use catalogue::Catalogue;
pub use check::Mismatch;
pub use family::{Cap, Family, Kind, Presence, DEFAULT_CAP, OVERFLOW};
pub use fixed::FixedCounts;
pub use labels::{Dim, Labels, Series};
pub use open::OpenRows;
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
            assert!($ty::ALL.len() == listed.len(), "ALL does not list every variant");
            let mut i = 0;
            while i < listed.len() {
                assert!($ty::ALL[i] as usize == i, "ALL is not in declaration order");
                assert!(listed[i] as usize == i, "the listed variants are not in declaration order");
                i += 1;
            }
        };
    };
}
