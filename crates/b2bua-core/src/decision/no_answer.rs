//! The ring deadline a routing answer states for the leg it dials: the one
//! resolver every route, failover, reroute and transfer target reads it
//! through.

use call::features::FeatureActivations;
use call::Deadline;

/// The ring deadline of a routing answer: the one it states, else its
/// features'. Absent, zero or negative is off; otherwise the milliseconds stated.
pub fn no_answer_deadline(stated: Option<i64>, features: Option<&FeatureActivations>) -> Deadline {
    Deadline::of_millis(stated.or(features.and_then(|f| f.no_answer_ms)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_stated_deadline_wins_over_the_features() {
        let mut features = crate::decision::default_platform_features();
        features.no_answer_ms = Some(20_000);
        assert_eq!(
            no_answer_deadline(Some(800), Some(&features)),
            Deadline::After(Duration::from_millis(800))
        );
        assert_eq!(
            no_answer_deadline(None, Some(&features)),
            Deadline::After(Duration::from_secs(20))
        );
    }

    #[test]
    fn zero_is_off_not_an_immediate_fire() {
        assert_eq!(no_answer_deadline(Some(0), None), Deadline::Off);
        assert_eq!(no_answer_deadline(None, None), Deadline::Off);
    }
}
