//! [`Switch`] — the guard's decision over the proxy's fitness readings: when
//! to refresh the set that lets the node's BFD and BGP out, and when to empty
//! it. Pure: the caller polls, writes the ruleset and hands both outcomes in
//! with the instant they happened.
//!
//! The kernel holds the dead-man rule: each refresh lets the traffic out for
//! the dead bound only (`ROUTE_GUARD_DEAD_MS`, the set's element timeout), so
//! a node whose guard or proxy stops refreshing it is withdrawn without any
//! write. The switch refreshes every quarter of that bound while the proxy
//! reads fit, empties the set at once on an unfit reading, and refreshes a
//! withdrawn node again only after the proxy read fit for the whole hold.

use std::time::Duration;

use tokio::time::Instant;

/// One reading of the proxy's fitness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    Fit,
    /// The proxy answered unfit, naming why.
    Unfit(String),
    /// No answer, or one that is neither: the proxy is gone, hung or slow.
    NoAnswer(String),
}

/// What the ruleset is to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// The peers let out for the dead bound from now.
    Refresh,
    /// No peer let out: the node's BFD and BGP to them are dropped.
    Withdraw,
}

/// Why the node was withdrawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// The proxy answered unfit for this reason.
    Unfit(String),
    /// No refresh within the dead bound: the proxy gave no fit answer, or the
    /// refreshes could not be written.
    Expired,
    /// The guard started with the proxy not fit.
    Start,
}

impl Cause {
    /// The metric label of the cause.
    pub fn label(&self) -> &'static str {
        match self {
            Cause::Unfit(_) => "unfit",
            Cause::Expired => "expired",
            Cause::Start => "start",
        }
    }
}

/// The switch's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How long one refresh lets the node's traffic out.
    pub dead: Duration,
    /// How long the proxy reads fit before a withdrawn node is let out.
    pub hold: Duration,
}

impl Timing {
    /// How often a fit proxy's set is refreshed: a quarter of the dead bound.
    pub fn refresh_every(&self) -> Duration {
        self.dead / 4
    }
}

/// What one reading asks of the caller.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Step {
    /// The ruleset to write now, if any.
    pub write: Option<Write>,
    /// The node withdrawn by this reading, and why.
    pub withdrawn: Option<Cause>,
}

/// The guard's view of its node.
#[derive(Debug)]
pub struct Switch {
    timing: Timing,
    withdrawn: bool,
    /// Since when the proxy reads fit without a break.
    fit_since: Option<Instant>,
    /// The last refresh written.
    refreshed_at: Option<Instant>,
}

impl Switch {
    /// The switch of a guard whose first reading is `first`, at `now`, and
    /// the ruleset it starts with: a fit proxy keeps its node out (no hold:
    /// a guard restart is no flap), any other reading withdraws it.
    pub fn start(timing: Timing, now: Instant, first: &Reading) -> (Self, Write, Option<Cause>) {
        let fit = *first == Reading::Fit;
        let switch =
            Self { timing, withdrawn: !fit, fit_since: fit.then_some(now), refreshed_at: None };
        if fit {
            (switch, Write::Refresh, None)
        } else {
            (switch, Write::Withdraw, Some(Cause::Start))
        }
    }

    /// Whether the guard reads its node withdrawn.
    pub fn withdrawn(&self) -> bool {
        self.withdrawn
    }

    /// The reading taken at `now`.
    pub fn on_reading(&mut self, now: Instant, reading: &Reading) -> Step {
        let mut step = Step::default();
        if !self.withdrawn && self.expired(now) {
            self.withdrawn = true;
            step.withdrawn = Some(Cause::Expired);
        }
        match reading {
            Reading::Fit => {
                let since = *self.fit_since.get_or_insert(now);
                let due = if self.withdrawn {
                    now.saturating_duration_since(since) >= self.timing.hold
                } else {
                    self.refreshed_at.is_none_or(|at| {
                        now.saturating_duration_since(at) >= self.timing.refresh_every()
                    })
                };
                if due {
                    step.write = Some(Write::Refresh);
                }
            }
            Reading::Unfit(reason) => {
                self.fit_since = None;
                if !self.withdrawn {
                    self.withdrawn = true;
                    step.write = Some(Write::Withdraw);
                    step.withdrawn = Some(Cause::Unfit(reason.clone()));
                }
            }
            Reading::NoAnswer(_) => self.fit_since = None,
        }
        step
    }

    /// A refresh was written at `now`: `true` when it lets a withdrawn node
    /// out again.
    pub fn refreshed(&mut self, now: Instant) -> bool {
        self.refreshed_at = Some(now);
        std::mem::replace(&mut self.withdrawn, false)
    }

    /// Whether the last refresh, if any, has run out at `now`.
    fn expired(&self, now: Instant) -> bool {
        self.refreshed_at.is_none_or(|at| now.saturating_duration_since(at) >= self.timing.dead)
    }
}

#[cfg(test)]
mod tests;
