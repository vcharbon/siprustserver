//! One catalogue entry: everything about a metric family but its values.

use crate::labels::Labels;

/// A Prometheus metric family as declared: the single place its name, kind,
/// label sets and help text are stated.
#[derive(Debug, Clone, Copy)]
pub struct Family {
    /// The metric name every series of the family carries; a histogram's
    /// samples carry it with `_bucket`, `_sum` and `_count` appended.
    pub name: &'static str,
    /// The `# TYPE` of the family.
    pub kind: Kind,
    /// Every label set the family declares; a histogram's `le` is not one.
    pub labels: Labels,
    /// Which series appear in the exposition, and when.
    pub presence: Presence,
    /// The `# HELP` text, unescaped.
    pub help: &'static str,
}

/// The `# TYPE` of a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A monotonic count.
    Counter,
    /// An instantaneous value.
    Gauge,
    /// Cumulative bucket counts (`_bucket{le}`), a `_sum` and a `_count` per
    /// label set.
    Histogram,
}

impl Kind {
    /// The `# TYPE` keyword.
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// Which series of a family appear in the exposition.
#[derive(Debug, Clone, Copy)]
pub enum Presence {
    /// Every declared label set is written on every scrape, at 0 before its
    /// first event, and no other.
    Fixed,
    /// Every declared label set is written on every scrape, at 0 before its
    /// first event; a label set nobody declared is written from its first
    /// observation on, after the declared ones, under its own labels.
    SemiOpen {
        /// The bound on undeclared label sets, where their values are not
        /// bounded by a protocol or by the code; `None` where they are.
        cap: Option<Cap>,
    },
}

/// The bound on a semi-open family's label sets whose values no protocol and
/// no code bounds.
#[derive(Debug, Clone, Copy)]
pub struct Cap {
    /// The labels whose values are unbounded (a method token, a peer).
    pub labels: &'static [&'static str],
    /// Label sets holding an undeclared value of one of [`labels`](Self::labels)
    /// kept under their own labels; past them an observation lands on an
    /// overflow series, those labels reading [`OVERFLOW`], every other one
    /// kept.
    pub max: usize,
    /// The counter of observations that landed on an overflow series.
    pub overflow: &'static Family,
}

/// The default [`Cap::max`].
pub const DEFAULT_CAP: usize = 256;

/// Every label value of a capped family's overflow series.
pub const OVERFLOW: &str = "_overflow";

impl Family {
    /// A counter with a fixed set of label sets.
    pub const fn counter(name: &'static str, labels: Labels, help: &'static str) -> Self {
        Self { name, kind: Kind::Counter, labels, presence: Presence::Fixed, help }
    }

    /// A gauge with a fixed set of label sets.
    pub const fn gauge(name: &'static str, labels: Labels, help: &'static str) -> Self {
        Self { name, kind: Kind::Gauge, labels, presence: Presence::Fixed, help }
    }

    /// A histogram with a fixed set of label sets.
    pub const fn histogram(name: &'static str, labels: Labels, help: &'static str) -> Self {
        Self { name, kind: Kind::Histogram, labels, presence: Presence::Fixed, help }
    }

    /// This family, semi-open with no cap: its undeclared label sets are
    /// bounded by a protocol or by the code.
    pub const fn semi_open(self) -> Self {
        Self { presence: Presence::SemiOpen { cap: None }, ..self }
    }

    /// This family, semi-open and capped at [`DEFAULT_CAP`] label sets with
    /// an undeclared value of one of `labels`, counting the excess on
    /// `overflow`.
    pub const fn capped(self, overflow: &'static Family, labels: &'static [&'static str]) -> Self {
        self.capped_at(DEFAULT_CAP, overflow, labels)
    }

    /// This family, semi-open and capped at `max` label sets with an
    /// undeclared value of one of `labels`, counting the excess on
    /// `overflow`.
    pub(crate) const fn capped_at(
        self,
        max: usize,
        overflow: &'static Family,
        labels: &'static [&'static str],
    ) -> Self {
        Self { presence: Presence::SemiOpen { cap: Some(Cap { labels, max, overflow }) }, ..self }
    }

    /// The cap of a semi-open family, if it has one.
    pub const fn cap(&self) -> Option<Cap> {
        match self.presence {
            Presence::Fixed => None,
            Presence::SemiOpen { cap } => cap,
        }
    }

    /// Whether a label set nobody declared may appear.
    pub const fn is_semi_open(&self) -> bool {
        matches!(self.presence, Presence::SemiOpen { .. })
    }
}
