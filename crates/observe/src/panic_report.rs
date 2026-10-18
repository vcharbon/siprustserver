//! The process panic hook: a panic is passed to the hook that was installed
//! before (the default one prints the message, the location and, when asked,
//! a backtrace) at most once per source location per [`REPORT_INTERVAL`];
//! the others at that location are only counted ([`counters::PANICS_REPEATED`]).
//! A panic a shared loop catches (ADR-0051 §5) can repeat at a peer's
//! datagram rate, and the default hook writes stderr synchronously on the
//! panicking task: one report per site per interval is the evidence, the
//! repeats are a count, and a later panic at the same site is still reported
//! once the interval has passed.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::counters;

/// How long a reported site's further panics are only counted.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// When each source location last reported a panic. Bounded by the code, not
/// by input: a location is a panic site compiled into the binary.
#[derive(Debug)]
pub struct PerLocation {
    interval: Duration,
    reported: Mutex<HashMap<(String, u32, u32), Instant>>,
}

impl PerLocation {
    /// A rate limit of one report per site per `interval`.
    pub fn new(interval: Duration) -> Self {
        Self { interval, reported: Mutex::new(HashMap::new()) }
    }

    /// Whether a panic at `location` at `now` is reported; one that is not is
    /// counted.
    pub fn report(&self, location: &Location<'_>, now: Instant) -> bool {
        let key = (location.file().to_owned(), location.line(), location.column());
        let mut reported = self.reported.lock().unwrap_or_else(PoisonError::into_inner);
        let due = reported
            .get(&key)
            .is_none_or(|last| now.saturating_duration_since(*last) >= self.interval);
        if due {
            reported.insert(key, now);
        } else {
            counters::bump(&counters::PANICS_REPEATED);
        }
        due
    }
}

/// Install the hook over the one in place, one report per site per
/// [`REPORT_INTERVAL`] on the system clock. A panic with no location is
/// always reported.
pub fn install() {
    install_with(REPORT_INTERVAL, Instant::now);
}

/// [`install`] with the interval and the clock given.
pub fn install_with(interval: Duration, now: impl Fn() -> Instant + Send + Sync + 'static) {
    let previous = std::panic::take_hook();
    let limit = PerLocation::new(interval);
    std::panic::set_hook(Box::new(move |info| {
        if info.location().is_none_or(|location| limit.report(location, now())) {
            previous(info);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn here() -> &'static Location<'static> {
        Location::caller()
    }

    #[test]
    fn a_site_reports_once_per_interval_and_its_repeats_are_counted() {
        let limit = PerLocation::new(Duration::from_secs(60));
        let (site, other) = (here(), here());
        let t0 = Instant::now();
        let before = counters::get(&counters::PANICS_REPEATED);
        assert!(limit.report(site, t0), "the first panic at a site is reported");
        assert!(!limit.report(site, t0 + Duration::from_secs(1)), "a repeat inside is not");
        assert!(!limit.report(site, t0 + Duration::from_secs(59)));
        assert!(limit.report(other, t0 + Duration::from_secs(1)), "another site on its own");
        assert!(limit.report(site, t0 + Duration::from_secs(60)), "past the interval, again");
        assert_eq!(counters::get(&counters::PANICS_REPEATED) - before, 2);
    }
}
