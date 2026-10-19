//! metric-values — the values a process holds for the metric families it
//! declares in a [`metric_catalogue::Catalogue`], written from any thread and
//! rendered at scrape time through the family's entry.
//!
//! - [`FixedCounts`] — a fixed family's counts, one lock-free counter per
//!   declared label set, keyed by label values.
//! - [`OpenRows`] — a semi-open family's rows: the declared ones, then any
//!   label set observed, under the family's cap.
//! - [`poison`] — the crate's one named lock, recovered and counted.

mod fixed;
mod open;
pub mod poison;

pub use fixed::FixedCounts;
pub use open::OpenRows;
