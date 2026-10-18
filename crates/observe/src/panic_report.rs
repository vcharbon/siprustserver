//! The process panic hook: the first panic at each source location goes to
//! the hook that was installed before (the default one prints the message,
//! the location and, when asked, a backtrace); a later panic at a location
//! already reported is only counted ([`counters::PANICS_REPEATED`]). A panic a
//! shared loop catches (ADR-0051 §5) can repeat at a peer's datagram rate, and
//! the default hook writes stderr synchronously on the panicking task: the
//! first report is the evidence, the repeats are a count.

use std::collections::HashSet;
use std::panic::Location;
use std::sync::{Mutex, PoisonError};

use crate::counters;

/// The source locations already reported. Bounded by the code, not by
/// input: a location is a panic site compiled into the binary.
#[derive(Debug, Default)]
pub struct FirstPerLocation {
    seen: Mutex<HashSet<(String, u32, u32)>>,
}

impl FirstPerLocation {
    /// Whether a panic at `location` is the first one there; a repeat is
    /// counted.
    pub fn first(&self, location: &Location<'_>) -> bool {
        let key = (location.file().to_owned(), location.line(), location.column());
        let first = self.seen.lock().unwrap_or_else(PoisonError::into_inner).insert(key);
        if !first {
            counters::bump(&counters::PANICS_REPEATED);
        }
        first
    }
}

/// Install the hook over the one in place. A panic with no location is
/// always reported.
pub fn install() {
    let previous = std::panic::take_hook();
    let seen = FirstPerLocation::default();
    std::panic::set_hook(Box::new(move |info| {
        if info.location().is_none_or(|location| seen.first(location)) {
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
    fn a_location_is_reported_once_and_its_repeats_are_counted() {
        let gate = FirstPerLocation::default();
        let (site, other) = (here(), here());
        let before = counters::get(&counters::PANICS_REPEATED);
        assert!(gate.first(site), "the first panic at a site is reported");
        assert!(!gate.first(site), "a repeat at the site is not");
        assert!(!gate.first(site));
        assert!(gate.first(other), "another site is reported on its own");
        assert_eq!(counters::get(&counters::PANICS_REPEATED) - before, 2);
    }
}
